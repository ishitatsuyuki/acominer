use std::collections::HashMap;
use std::convert::TryInto;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::Context as _;
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;
use byteorder::{BigEndian, ByteOrder};
use bytes::{BufMut, BytesMut};
use once_cell::sync::Lazy;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::io::{BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace};

use crate::cache::epoch_lut;

static EPOCH_LUT: Lazy<HashMap<[u8; 32], usize>> = Lazy::new(|| {
    // epoch 1024 is far ahead in date
    epoch_lut(1024)
});

struct ProtocolCommon {
    req_id: u64,
    req_buf: BytesMut,
    req_notify: Option<Waker>,
    res_buf: Vec<u8>,
    rx: BufReader<OwnedReadHalf>,
    tx: BufWriter<OwnedWriteHalf>,
    current_job: Arc<Mutex<JobInfo>>,
    user_agent: String,
    hostname: String,
    port: u16,
    username: String,
    password: String,
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
    fn set_difficulty(&mut self, boundary_truncated: u64) {
        self.difficulty = boundary_truncated;
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

impl StratumClient {
    pub fn new(user_agent: &str, hostname: &str, port: u16, username: &str, password: &str) -> anyhow::Result<StratumClient> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let current_job = Arc::new(Mutex::new(JobInfo {
            received: Instant::now(),
            difficulty: 0x00000000FFFF0000,
            extra_nonce: 0,
            pool_nonce_bytes: 0,
            job_id: "".to_string(),
            epoch: 0,
            header_hash: vec![],
        }));
        let mut backend = NiceHashStratum::default();
        let mut inner = rt.block_on(Self::new_impl(hostname, port, current_job.clone(), user_agent, username, password, &mut backend))?;
        let (stop_tx, stop_rx) = oneshot::channel();
        let (submissions_tx, submissions_rx) = mpsc::channel(16);

        let thread = {
            std::thread::spawn(move || {
                rt.block_on(inner.worker(submissions_rx, stop_rx, &mut backend))
            })
        };
        Ok(StratumClient {
            thread: Some(thread),
            current_job,
            submissions: submissions_tx,
            stop: Some(stop_tx),
        })
    }

    async fn new_impl(hostname: &str, port: u16, current_job: Arc<Mutex<JobInfo>>, user_agent: &str, username: &str, password: &str, backend: &mut impl ProtocolBackend)
                      -> anyhow::Result<ProtocolCommon> {
        let conn = TcpStream::connect((hostname, port)).await?;
        conn.set_nodelay(true)?;
        let (rx, tx) = conn.into_split();
        let rx = BufReader::new(rx);
        let tx = BufWriter::new(tx);
        let mut inner = ProtocolCommon {
            rx,
            tx,
            req_id: 0,
            current_job,
            user_agent: user_agent.to_owned(),
            username: username.to_owned(),
            password: password.to_owned(),
            hostname: hostname.to_owned(),
            port,
            req_buf: BytesMut::new(),
            req_notify: None,
            res_buf: Vec::new(),
        };
        debug!("Connecting to pool");
        backend.connect(&mut inner)?;
        while inner.current_job.lock().unwrap().epoch == 0 {
            inner.send_recv(None, backend).await?;
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

impl ProtocolCommon {
    fn send_recv<'a>(&'a mut self, mut lock: Option<&'a mut JobInfo>, backend: &'a mut impl ProtocolBackend) -> impl Future<Output=anyhow::Result<()>> + 'a {
        futures::future::poll_fn(move |cx: &mut Context| {
            trace!("send_recv begin");
            let read_until = self.rx.read_until(b'\n', &mut self.res_buf);
            futures::pin_mut!(read_until);
            let res = read_until.poll(cx);

            let mut ready = false;

            if let Poll::Ready(r) = res {
                let _ = r?;
                let buf = std::mem::replace(&mut self.res_buf, vec![]);
                trace!("Handling incoming message");
                if buf.is_empty() {
                    return Poll::Ready(Err(anyhow::anyhow!("Connection terminated unexpectedly")));
                } else {
                    ready = true;
                    backend.handle_incoming(self, lock.as_deref_mut(), &buf[..buf.len() - 1])?;
                }
            }

            if self.req_buf.is_empty() {
                let _ = futures::ready!(Pin::new(&mut self.tx).poll_flush(cx))?; // Note: TCP flush doesn't actually exist, so this is no-op for now
                self.req_notify = Some(cx.waker().clone());
            } else {
                let write_all_buf = self.tx.write_all_buf(&mut self.req_buf);
                futures::pin_mut!(write_all_buf);
                let res = write_all_buf.poll(cx);
                if let Poll::Ready(r) = res {
                    let _ = r?;
                    ready = true;
                }
            }
            if ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
    }

    async fn worker(&mut self, mut submissions: mpsc::Receiver<(String, String, Instant)>, mut stop: oneshot::Receiver<()>, backend: &mut impl ProtocolBackend) {
        let mut closed = false;
        let mut submission_flushed = false;
        loop {
            if submission_flushed && backend.outstanding() == 0 {
                return;
            }
            tokio::select! {
                val = self.send_recv(None, backend) => {
                    match val {
                        Ok(()) => {},
                        Err(e) => {
                            error!("Disconnected! {:?}\nBlocking computation until connection is recovered...", e);
                            let clone = self.current_job.clone();
                            let mut guard = clone.lock().unwrap();

                            let mut backoff = ExponentialBackoff::default();

                            while let Err(e) = self.reconnect(&mut guard, backend).await {
                                error!("Error while reconnecting, retrying: {}", e);
                                tokio::time::sleep(backoff.next_backoff().unwrap()).await;
                            }
                            info!("Reconnected, resuming computation.");
                        }
                    }
                }
                val = submissions.recv(), if !submission_flushed => {
                    if let Some((job_id, nonce_hex, ts)) = val {
                        if let Err(e) = backend.submit(self, Instant::now(), ts, &job_id, &nonce_hex) {
                            error!("Error submitting share: {}", e);
                        }
                    } else {
                        submission_flushed = true;
                    }
                }
                val = &mut stop, if !closed => {
                    val.unwrap();
                    submissions.close();
                    closed = true;
                }
            }
        }
    }

    fn new_id(&mut self) -> u64 {
        self.req_id += 1;
        self.req_id
    }

    fn send(&mut self, request: Vec<u8>) {
        self.req_buf.extend_from_slice(&request);
        self.req_buf.put_u8(b'\n');
        if let Some(w) = self.req_notify.take() {
            w.wake();
        };
    }

    fn submit_complete(&mut self, job_time: Instant, send_time: Instant, job_id: &str, result: anyhow::Result<()>) {
        match result {
            Ok(()) => info!(job_id, ping = ?send_time.elapsed(), time_since_job = ?job_time.elapsed(), "Share accepted"),
            Err(e) => error!(job_id, "Share rejected: {:?}", e),
        }
    }

    async fn reconnect(&mut self, lock: &mut JobInfo, backend: &mut impl ProtocolBackend) -> anyhow::Result<()> {
        let conn = TcpStream::connect((&*self.hostname, self.port)).await?;
        conn.set_nodelay(true)?;
        let (rx, tx) = conn.into_split();
        self.rx = BufReader::new(rx);
        self.tx = BufWriter::new(tx);

        lock.epoch = 0;
        self.req_buf.clear();
        backend.connect(self)?;
        while lock.epoch == 0 {
            self.send_recv(Some(lock), backend).await?;
        }
        Ok(())
    }
}

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
        pub id: Option<Value>,
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

trait ProtocolBackend {
    fn connect(&mut self, common: &mut ProtocolCommon) -> anyhow::Result<()>;
    fn submit(&mut self, common: &mut ProtocolCommon, send_time: Instant, job_time: Instant, job_id: &str, nonce_hex: &str) -> anyhow::Result<()>;

    fn handle_incoming(&mut self, common: &mut ProtocolCommon, lock: Option<&mut JobInfo>, buf: &[u8]) -> anyhow::Result<()>;
    fn outstanding(&mut self) -> usize;
}

/// The NiceHashStratum/1.0.0 protocol.
/// Documented at https://github.com/nicehash/Specifications/blob/master/EthereumStratum_NiceHash_v1.0.0.txt
#[derive(Default, Debug)]
struct NiceHashStratum {
    requests: HashMap<u64, NiceHashStratumOutgoing>,
}

#[derive(Debug)]
enum NiceHashStratumOutgoing {
    Subscribe,
    Authorize,
    Submit {
        send_time: Instant,
        job_time: Instant,
        job_id: String,
    },
}

impl ProtocolBackend for NiceHashStratum {
    fn connect(&mut self, common: &mut ProtocolCommon) -> anyhow::Result<()> {
        let id = common.new_id();
        let subscribe = jsonrpc::Request {
            method: "mining.subscribe".into(),
            params: vec![
                common.user_agent.to_owned().into(), "EthereumStratum/1.0.0".into(),
            ],
            id: Some(id.into()),
        };
        common.send(serde_json::to_vec(&subscribe)?);
        self.requests.insert(id, NiceHashStratumOutgoing::Subscribe);
        let id = common.new_id();
        let authorize = jsonrpc::Request {
            method: "mining.authorize".into(),
            params: vec![
                common.username.as_str().into(), common.password.as_str().into(),
            ],
            id: Some(id.into()),
        };
        common.send(serde_json::to_vec(&authorize)?);
        self.requests.insert(id, NiceHashStratumOutgoing::Authorize);
        Ok(())
    }

    fn submit(&mut self, common: &mut ProtocolCommon, send_time: Instant, job_time: Instant, job_id: &str, nonce_hex: &str) -> anyhow::Result<()> {
        let id = common.new_id();
        let submit = jsonrpc::Request {
            method: "mining.submit".into(),
            params: vec![
                common.username.as_str().into(), job_id.into(), nonce_hex.into(),
            ],
            id: Some(id.into()),
        };
        common.send(serde_json::to_vec(&submit)?);
        self.requests.insert(id, NiceHashStratumOutgoing::Submit { send_time, job_time, job_id: job_id.to_owned() });
        Ok(())
    }

    fn handle_incoming(&mut self, common: &mut ProtocolCommon, mut lock: Option<&mut JobInfo>, buf: &[u8]) -> anyhow::Result<()> {
        if let Ok(req) = serde_json::from_slice::<jsonrpc::Request>(buf) {
            let clone = common.current_job.clone();
            let mut local_lock = None;
            let job = lock.unwrap_or_else(|| {
                local_lock = Some(clone.lock().unwrap());
                local_lock.as_mut().unwrap()
            });
            anyhow::ensure!(req.id.map(|x|x.is_null()).unwrap_or(true));
            match &*req.method {
                "mining.set_difficulty" => {
                    // Flawed, but works
                    let d = req.params[0].as_f64().unwrap();
                    job.set_difficulty((d.recip() * 0x00000000ffff0000u64 as f64) as u64);
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
        } else {
            let res: jsonrpc::Response = serde_json::from_slice(buf)?;
            if !res.id.is_null() {
                let id = res.id.as_u64().context("`id` in response should be an integer")?;
                match self.requests.remove(&id).context("Unknown id")? {
                    NiceHashStratumOutgoing::Subscribe => {
                        let res = res.result.as_ref().and_then(|x| x.as_array()).context("`result` should be an array")?;
                        let inner = res.get(0).and_then(|x| x.as_array()).context("`result[0]` should be an array")?;
                        anyhow::ensure!(inner[0].as_str() == Some("mining.notify"));
                        let mut start_nonce = hex::decode(res.get(1).and_then(|x|x.as_str()).context("`result[1]` should be a string")?)?;
                        let pool_nonce_bytes = start_nonce.len();
                        start_nonce.resize(8, 0);
                        let start_nonce = BigEndian::read_u64(&start_nonce);
                        let clone = common.current_job.clone();
                        let mut local_lock = None;
                        let mut job = lock.as_deref_mut().unwrap_or_else(|| {
                            local_lock = Some(clone.lock().unwrap());
                            local_lock.as_mut().unwrap()
                        });
                        job.extra_nonce = start_nonce;
                        job.pool_nonce_bytes = pool_nonce_bytes;
                        drop(local_lock);
                    }
                    NiceHashStratumOutgoing::Authorize => {
                        anyhow::ensure!(res.result == Some(Value::Bool(true)), "Authentication failed: {:?}", res.error);
                    }
                    NiceHashStratumOutgoing::Submit { send_time, job_time, job_id } => {
                        let res = match res.result {
                            Some(Value::Bool(true)) => Ok(()),
                            // There are a million of ways to return an error:
                            // - NiceHash Stratum specifies result as "false" on error. Ethermine follows this.
                            // - But JSON-RPC requires result to be null on error. Binance pool seems to follow this instead.
                            // - It can also be completely left out. This is JSON-RPC 2.0 behavior. MoneroOcean does this.
                            Some(Value::Bool(false)) | Some(Value::Null) | None => Err(anyhow::anyhow!("{:?}", res.error)),
                            _ => anyhow::bail!("mining.submit result should be a bool"),
                        };
                        common.submit_complete(job_time, send_time, &job_id, res);
                    }
                }
            }
        }
        Ok(())
    }

    fn outstanding(&mut self) -> usize {
        self.requests.len()
    }
}
