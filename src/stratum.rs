use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle};

use byteorder::{BigEndian, ByteOrder};
use serde_derive::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};

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

#[derive(Clone, Default, Debug)]
pub struct JobInfo {
    pub difficulty: u64,
    pub extra_nonce: u64,
    pub pool_nonce_bytes: usize,
    pub job_id: String,
    pub seed_hash: Vec<u8>,
    pub header_hash: Vec<u8>,
}

pub struct StratumClient {
    thread: Option<JoinHandle<()>>,
    current_job: Arc<Mutex<JobInfo>>,
    submissions: mpsc::Sender<(String, String)>,
    stop: Option<oneshot::Sender<()>>,
}

struct ClientInner {
    req_id: usize,
    rx: BufReader<OwnedReadHalf>,
    tx: BufWriter<OwnedWriteHalf>,
    current_job: Arc<Mutex<JobInfo>>,
    username: String,
}

impl StratumClient {
    pub fn new(user_agent: &str, target: impl ToSocketAddrs, username: &str, password: &str) -> std::io::Result<StratumClient> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all()
            .build()?;
        let current_job = Arc::new(Mutex::new(JobInfo {
            difficulty: 0x00000000FFFF0000,
            ..Default::default()
        }));
        let mut inner = rt.block_on(Self::new_impl(target, current_job.clone(), user_agent, username, password))?;
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

    async fn new_impl(target: impl ToSocketAddrs, current_job: Arc<Mutex<JobInfo>>, user_agent: &str, username: &str, password: &str)
                      -> std::io::Result<ClientInner> {
        let conn = TcpStream::connect(target).await?;
        conn.set_nodelay(true)?;
        let (rx, tx) = conn.into_split();
        let rx = BufReader::new(rx);
        let tx = BufWriter::new(tx);
        let mut inner = ClientInner { rx, tx, req_id: 0, current_job, username: username.to_owned() };
        inner.connect(user_agent, username, password).await?;
        while inner.current_job.lock().unwrap().seed_hash.is_empty() {
            assert!(inner.handle_notification().await?.is_none());
        }
        Ok(inner)
    }

    pub fn submit(&mut self, job_id: String, nonce: u64, pool_nonce_bytes: usize) {
        let mut buf = [0; 8];
        BigEndian::write_u64(&mut buf, nonce);
        let nonce_hex = hex::encode(&buf[pool_nonce_bytes..]);
        self.submissions.try_send((job_id, nonce_hex)).unwrap();
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
    async fn handle_notification(&mut self) -> std::io::Result<Option<Response>> {
        let mut buf = vec![];
        let _count = self.rx.read_until(b'\n', &mut buf).await?;

        if let Ok(req) = serde_json::from_slice::<Request>(&buf[..buf.len() - 1]) {
            let mut job = self.current_job.lock().unwrap();
            assert!(req.id.is_null());
            match &*req.method {
                "mining.set_difficulty" => {
                    // Flawed, but works
                    let d = req.params[0].as_f64().unwrap();
                    job.difficulty = (d.recip() * 0x00000000ffff0000u64 as f64) as u64;
                }
                "mining.notify" => {
                    let job_id = req.params[0].as_str().unwrap().to_owned();
                    let seed_hash = hex::decode(req.params[1].as_str().unwrap()).unwrap();
                    let header_hash = hex::decode(req.params[2].as_str().unwrap()).unwrap();
                    // clean_jobs is unimplemented
                    job.job_id = job_id;
                    job.seed_hash = seed_hash;
                    job.header_hash = header_hash;
                }
                _ => unimplemented!(),
            }
            Ok(None)
        } else {
            let resp: Response = serde_json::from_slice(&buf[..buf.len() - 1]).unwrap(); // TODO: error handling
            Ok(Some(resp))
        }
    }

    /// Block until a response is received.
    async fn get_response(&mut self) -> std::io::Result<Response> {
        loop {
            let res = self.handle_notification().await?;
            if let Some(res) = res {
                return Ok(res);
            }
        }
    }

    async fn worker(&mut self, mut submissions: mpsc::Receiver<(String, String)>, mut stop: oneshot::Receiver<()>) {
        loop {
            tokio::select! {
                val = self.handle_notification() => { assert!(val.unwrap().is_none()); }
                val = submissions.recv() => { if let Some((job_id, nonce_hex)) = val {
                    self.submit(&job_id, &nonce_hex).await.unwrap();
                }}
                val = &mut stop => {
                    val.unwrap();
                    submissions.close();
                    while let Some((job_id, nonce_hex)) = submissions.recv().await {
                        self.submit(&job_id, &nonce_hex).await.unwrap();
                    }
                    return;
                }
            }
        }
    }

    async fn request(&mut self, mut req: Request) -> std::io::Result<Response> {
        self.req_id += 1;
        req.id = self.req_id.into();
        let req = serde_json::to_vec(&req).unwrap();
        self.tx.write_all(&req).await?;
        self.tx.write_u8(b'\n').await?;
        self.tx.flush().await?;
        self.get_response().await
    }

    async fn connect(&mut self, user_agent: &str, username: &str, password: &str) -> std::io::Result<()> {
        let subscribe = Request {
            method: "mining.subscribe".into(),
            params: vec![
                user_agent.to_owned().into(), "EthereumStratum/1.0.0".into()
            ],
            id: 0.into(),
        };
        let res = self.request(subscribe).await?;
        let res = res.result.unwrap();
        let res = res.as_array().unwrap()[0].as_array().unwrap();
        assert_eq!(res[0].as_str().unwrap(), "mining.notify");
        let mut start_nonce = hex::decode(res[1].as_str().unwrap()).unwrap();
        let pool_nonce_bytes = start_nonce.len();
        start_nonce.resize(8, 0);
        let start_nonce = BigEndian::read_u64(&start_nonce);
        let mut job = self.current_job.lock().unwrap();
        job.extra_nonce = start_nonce;
        job.pool_nonce_bytes = pool_nonce_bytes;
        drop(job);

        let authorize = Request {
            method: "mining.authorize".into(),
            params: vec![
                username.into(), password.into()
            ],
            id: 0.into(),
        };

        let res = self.request(authorize).await?;
        let res = res.result.unwrap();
        assert!(res.as_bool().unwrap());
        Ok(())
    }

    async fn submit(&mut self, job_id: &str, nonce_hex: &str) -> std::io::Result<()> {
        let submit = Request {
            method: "mining.submit".into(),
            params: vec![
                self.username.clone().into(), job_id.into(), nonce_hex.into(),
            ],
            id: self.req_id.into(),
        };
        let res = self.request(submit).await?;
        let result = res.result.unwrap().as_bool().unwrap();
        if result {
            println!("Share submitted");
        } else {
            println!("Share rejected: {:?}", res.error);
        }
        Ok(())
    }
}
