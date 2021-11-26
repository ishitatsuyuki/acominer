use std::collections::HashMap;
use std::convert::TryInto;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;
use byteorder::{BigEndian, ByteOrder};
use once_cell::sync::Lazy;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::io::{BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::cache::epoch_lut;

static EPOCH_LUT: Lazy<HashMap<[u8; 32], usize>> = Lazy::new(|| {
    // at epoch 4096 the dag size should have already exploded out of control, so it should be fine
    epoch_lut(4096)
});

mod jsonrpc {
    use serde_derive::{Deserialize, Serialize};
    use serde_json::Value;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    /// A JSON-RPC request object
    pub struct Request {
        /// A String containing the name of the method to be invoked
        pub method: String,
        /// An Array of objects to pass as arguments to the method
        pub params: Vec<Value>,
        /// The request id. This can be of any type. It is used to match the
        /// response with the request that it is replying to
        pub id: Value,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    /// A JSON-RPC response object
    pub struct Response {
        /// The Object that was returned by the invoked method. This must be null
        /// in case there was a error invoking the method
        pub result: Option<Value>,
        /// An Error object if there was an error invoking the method. It must be
        /// null if there was no error
        pub error: Option<Value>,
        /// This must be the same id as the request it is responding to
        pub id: Value,
    }
}

#[derive(Clone, Debug)]
pub struct JobInfo {
    pub received: Instant,
    pub difficulty: u64,
    pub extra_nonce: u64,
    pub pool_nonce_bytes: usize,
    pub job_id: String,
    pub epoch: usize,
    pub header_hash: Vec<u8>,
}

impl JobInfo {
    fn set_difficulty(&mut self, scale: f64) {
        self.difficulty = (scale.recip() * 0x00000000ffff0000u64 as f64) as u64;
        info!(difficulty = %format!("{:.2}G", 64f64.exp2() / self.difficulty as f64 / 1000f64.powf(3.0)), "Pool set difficulty");
    }

    fn set_job(&mut self, job_id: String, epoch: usize, header_hash: Vec<u8>) {
        debug!(%job_id, "Job received");
        self.job_id = job_id;
        self.epoch = epoch;
        self.header_hash = header_hash;
        self.received = Instant::now();
    }
}

pub struct StratumClient {
    thread: Option<JoinHandle<()>>,
    current_job: Arc<Mutex<JobInfo>>,
    submissions: mpsc::Sender<(String, String, Instant)>,
    stop: Option<oneshot::Sender<()>>,
}

struct ClientInner {
    req_id: usize,
    rx: BufReader<OwnedReadHalf>,
    tx: BufWriter<OwnedWriteHalf>,
    current_job: Arc<Mutex<JobInfo>>,
    user_agent: String,
    hostname: String,
    port: u16,
    username: String,
    password: String,
}

impl StratumClient {
    pub fn new(user_agent: &str, hostname: &str, port: u16, username: &str, password: &str) -> anyhow::Result<StratumClient> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all()
            .build()?;
        let current_job = Arc::new(Mutex::new(JobInfo {
            received: Instant::now(),
            difficulty: 0x00000000FFFF0000,
            extra_nonce: 0,
            pool_nonce_bytes: 0,
            job_id: "".to_string(),
            epoch: 0,
            header_hash: vec![],
        }));
        let mut inner = rt.block_on(Self::new_impl(hostname, port, current_job.clone(), user_agent, username, password))?;
        let (stop_tx, stop_rx) = oneshot::channel();
        let (submissions_tx, submissions_rx) = mpsc::channel(16);

        let thread = {
            std::thread::spawn(move || {
                rt.block_on(inner.worker(submissions_rx, stop_rx))
            })
        };
        Ok(StratumClient {
            thread: Some(thread),
            current_job,
            submissions: submissions_tx,
            stop: Some(stop_tx),
        })
    }

    async fn new_impl(hostname: &str, port: u16, current_job: Arc<Mutex<JobInfo>>, user_agent: &str, username: &str, password: &str)
                      -> anyhow::Result<ClientInner> {
        let conn = TcpStream::connect((hostname, port)).await?;
        conn.set_nodelay(true)?;
        let (rx, tx) = conn.into_split();
        let rx = BufReader::new(rx);
        let tx = BufWriter::new(tx);
        let mut inner = ClientInner {
            rx,
            tx,
            req_id: 0,
            current_job,
            user_agent: user_agent.to_owned(),
            username: username.to_owned(),
            password: password.to_owned(),
            hostname: hostname.to_owned(),
            port,
        };
        inner.connect(None).await?;
        while inner.current_job.lock().unwrap().epoch == 0 {
            assert!(inner.handle_notification(None).await?.is_none());
        }
        Ok(inner)
    }

    pub fn submit(&mut self, job: &JobInfo, nonce: u64) {
        let mut buf = [0; 8];
        BigEndian::write_u64(&mut buf, nonce);
        let nonce_hex = hex::encode(&buf[job.pool_nonce_bytes..]);
        self.submissions.try_send((job.job_id.clone(), nonce_hex, job.received.clone())).unwrap();
    }

    pub fn current_job(&self) -> JobInfo {
        let job = self.current_job.lock().unwrap().clone();
        job
    }
}

impl Drop for StratumClient {
    fn drop(&mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.thread.take().unwrap().join().unwrap();
    }
}

impl ClientInner {
    // Handle server side messages; return None if the message was processed, return the response
    // if the message was a response to a client request.
    async fn handle_notification(&mut self, lock: Option<&mut JobInfo>) -> anyhow::Result<Option<jsonrpc::Response>> {
        let mut buf = vec![];
        let _count = self.rx.read_until(b'\n', &mut buf).await?;

        if buf.is_empty() {
            anyhow::bail!("Connection terminated unexpectedly");
        } else if let Ok(req) = serde_json::from_slice::<jsonrpc::Request>(&buf[..buf.len() - 1]) {
            let clone = self.current_job.clone();
            let mut local_lock = None;
            let job = lock.unwrap_or_else(|| {
                local_lock = Some(clone.lock().unwrap());
                local_lock.as_mut().unwrap()
            });
            anyhow::ensure!(req.id.is_null());
            match &*req.method {
                "mining.set_difficulty" => {
                    // Flawed, but works
                    let d = req.params[0].as_f64().unwrap();
                    job.set_difficulty(d);
                }
                "mining.notify" => {
                    let job_id = req.params[0].as_str().unwrap().to_owned();
                    let seed_hash: [u8; 32] = hex::decode(req.params[1].as_str().unwrap())?.try_into().unwrap();
                    let epoch = *EPOCH_LUT.get(&seed_hash).ok_or_else(|| anyhow::anyhow!("Cannot resolve seed hash"))?;
                    let header_hash = hex::decode(req.params[2].as_str().unwrap())?;
                    // clean_jobs is unimplemented
                    job.set_job(job_id, epoch, header_hash);
                }
                _ => anyhow::bail!("Unsupported stratum request {}", req.method),
            }
            Ok(None)
        } else {
            let resp: jsonrpc::Response = serde_json::from_slice(&buf[..buf.len() - 1])?;
            Ok(Some(resp))
        }
    }

    /// Block until a response is received.
    async fn get_response(&mut self, mut lock: Option<&mut JobInfo>) -> anyhow::Result<jsonrpc::Response> {
        loop {
            let res = self.handle_notification(lock.as_deref_mut()).await?;
            if let Some(res) = res {
                return Ok(res);
            }
        }
    }

    async fn worker(&mut self, mut submissions: mpsc::Receiver<(String, String, Instant)>, mut stop: oneshot::Receiver<()>) {
        loop {
            tokio::select! {
                val = self.handle_notification(None) => {
                    match val {
                        Ok(None) => {},
                        _ => {
                            error!("Disconnected, blocking computation until connection is recovered...");
                            let clone = self.current_job.clone();
                            let mut guard = clone.lock().unwrap();

                            let mut backoff = ExponentialBackoff::default();

                            while let Err(e) = self.reconnect(&mut guard).await {
                                error!("Error while reconnecting, retrying: {}", e);
                                tokio::time::sleep(backoff.next_backoff().unwrap()).await;
                            }
                            info!("Reconnected, resuming computation.");
                        }
                    }
                }
                val = submissions.recv() => {
                    if let Some((job_id, nonce_hex, ts)) = val {
                        if let Err(e) = self.submit(&job_id, &nonce_hex, ts).await {
                            error!("Error submitting share: {}", e);
                        }
                    }
                }
                val = &mut stop => {
                    val.unwrap();
                    submissions.close();
                    while let Some((job_id, nonce_hex, ts)) = submissions.recv().await {
                        if let Err(e) = self.submit(&job_id, &nonce_hex, ts).await {
                            error!("Error submitting share: {}", e);
                        }
                    }
                    return;
                }
            }
        }
    }

    async fn request(&mut self, mut req: jsonrpc::Request, lock: Option<&mut JobInfo>) -> anyhow::Result<jsonrpc::Response> {
        self.req_id += 1;
        req.id = self.req_id.into();
        let req = serde_json::to_vec(&req).unwrap();
        self.tx.write_all(&req).await?;
        self.tx.write_u8(b'\n').await?;
        self.tx.flush().await?;
        self.get_response(lock).await
    }

    async fn connect(&mut self, mut lock: Option<&mut JobInfo>) -> anyhow::Result<()> {
        let subscribe = jsonrpc::Request {
            method: "mining.subscribe".into(),
            params: vec![
                self.user_agent.to_owned().into(), "EthereumStratum/1.0.0".into(),
            ],
            id: 0.into(),
        };
        let res = self.request(subscribe, lock.as_deref_mut()).await?;
        let res = res.result.unwrap();
        let res = res.as_array().unwrap()[0].as_array().unwrap();
        assert_eq!(res[0].as_str().unwrap(), "mining.notify");
        let mut start_nonce = hex::decode(res[1].as_str().unwrap()).unwrap();
        let pool_nonce_bytes = start_nonce.len();
        start_nonce.resize(8, 0);
        let start_nonce = BigEndian::read_u64(&start_nonce);
        let clone = self.current_job.clone();
        let mut local_lock = None;
        let mut job = lock.as_deref_mut().unwrap_or_else(|| {
            local_lock = Some(clone.lock().unwrap());
            local_lock.as_mut().unwrap()
        });
        job.extra_nonce = start_nonce;
        job.pool_nonce_bytes = pool_nonce_bytes;
        drop(local_lock);

        let authorize = jsonrpc::Request {
            method: "mining.authorize".into(),
            params: vec![
                self.username.as_str().into(), self.password.as_str().into(),
            ],
            id: 0.into(),
        };

        let res = self.request(authorize, lock.as_deref_mut()).await?;
        let res = res.result.unwrap();
        assert!(res.as_bool().unwrap());
        Ok(())
    }

    async fn reconnect(&mut self, lock: &mut JobInfo) -> anyhow::Result<()> {
        let conn = TcpStream::connect((&*self.hostname, self.port)).await?;
        conn.set_nodelay(true)?;
        let (rx, tx) = conn.into_split();
        self.rx = BufReader::new(rx);
        self.tx = BufWriter::new(tx);

        self.connect(Some(lock)).await?;
        while lock.epoch == 0 {
            assert!(self.handle_notification(Some(lock)).await?.is_none());
        }
        Ok(())
    }

    async fn submit(&mut self, job_id: &str, nonce_hex: &str, received_ts: Instant) -> anyhow::Result<()> {
        let submit = jsonrpc::Request {
            method: "mining.submit".into(),
            params: vec![
                self.username.clone().into(), job_id.into(), nonce_hex.into(),
            ],
            id: self.req_id.into(),
        };
        let before_send = Instant::now();
        let res = self.request(submit, None).await?;
        let result = match res.result {
            Some(Value::Bool(x)) => x,
            None => false, // NiceHash Stratum specifies result as "false" on error but JSON-RPC requires result to be null
            _ => anyhow::bail!("mining.submit result should be a bool"),
        };
        if result {
            info!(ping = ?before_send.elapsed(), time_since_job=?received_ts.elapsed(), "Share accepted");
        } else {
            error!("Share rejected: {:?}", res.error);
        }
        Ok(())
    }
}
