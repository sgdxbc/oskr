use std::{
    collections::HashMap, convert::Infallible, fs::File, hash::Hash, io::Read, path::Path,
    str::FromStr,
};

use crate::common::{ReplicaId, SigningKey, VerifyingKey, ViewNumber};

pub trait Transport
where
    Self: 'static,
{
    type Address: Clone + Eq + Hash + Send + Sync;
    type RxBuffer: AsRef<[u8]> + Send;
    // TxAgent has to be Sync for now, because it may show up in stage's shared
    // state and be accessed concurrently from multiple threads
    // this may be bad for two reason: it was designed as Send + !Sync in mind
    // from beginning; if in the future we want to reimplement dpdk transport,
    // which promise there is at most one TxAgent instance per thread, then it
    // may cache worker id and become !Sync for real
    // (or maybe just put it into thread local data?)
    type TxAgent: TxAgent<Transport = Self> + Clone + Send + Sync;

    fn tx_agent(&self) -> Self::TxAgent;

    fn register(
        &mut self,
        receiver: &impl Receiver<Self>,
        rx_agent: impl Fn(Self::Address, Self::RxBuffer) + 'static + Send,
    ) where
        Self: Sized;
    fn register_multicast(
        &mut self,
        rx_agent: impl Fn(Self::Address, Self::RxBuffer) + 'static + Send,
    );

    fn ephemeral_address(&self) -> Self::Address;
}

pub trait Receiver<T: Transport> {
    fn get_address(&self) -> &T::Address;
    // anything else?
}

pub trait TxAgent {
    type Transport: Transport;

    fn config(&self) -> &Config<Self::Transport>;

    fn send_message(
        &self,
        source: &impl Receiver<Self::Transport>,
        dest: &<Self::Transport as Transport>::Address,
        message: impl FnOnce(&mut [u8]) -> u16,
    );
    fn send_message_to_replica(
        &self,
        source: &impl Receiver<Self::Transport>,
        replica_id: ReplicaId,
        message: impl FnOnce(&mut [u8]) -> u16,
    ) {
        self.send_message(
            source,
            &self.config().replica_address[replica_id as usize],
            message,
        );
    }
    fn send_message_to_all(
        &self,
        source: &impl Receiver<Self::Transport>,
        message: impl FnOnce(&mut [u8]) -> u16,
    );
    fn send_message_to_multicast(
        &self,
        source: &impl Receiver<Self::Transport>,
        message: impl FnOnce(&mut [u8]) -> u16,
    ) {
        self.send_message(
            source,
            self.config().multicast_address.as_ref().unwrap(),
            message,
        );
    }
}

// consider move to dedicated module if getting too long
pub struct Config<T: Transport + ?Sized> {
    pub replica_address: Vec<T::Address>,
    pub multicast_address: Option<T::Address>,
    pub n_fault: usize,
    // for non-signed protocol this is empty
    pub signing_key: HashMap<T::Address, SigningKey>,
}

impl<T: Transport + ?Sized> Config<T> {
    pub fn verifying_key(&self) -> HashMap<T::Address, VerifyingKey> {
        self.signing_key
            .iter()
            .map(|(address, key)| (address.clone(), key.verifying_key()))
            .collect()
    }

    pub fn view_primary(&self, view_number: ViewNumber) -> ReplicaId {
        (view_number as usize % self.replica_address.len()) as ReplicaId
    }
}

impl<T: Transport + ?Sized> FromStr for Config<T>
where
    T::Address: FromStr,
{
    type Err = Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut replica_address = Vec::new();
        let mut multicast_address = None;
        let mut n_fault = None;
        for line in s.lines() {
            let line = if let Some((line, _)) = line.split_once('#') {
                line.trim()
            } else {
                line.trim()
            };
            if line.is_empty() {
                continue;
            }
            let (prompt, value) = line.split_once(char::is_whitespace).unwrap();
            let error_message = "failed to parse replica address";
            match prompt {
                "f" => n_fault = Some(value.parse().unwrap()),
                "replica" => {
                    replica_address.push(value.parse().map_err(|_| error_message).unwrap())
                }
                "multicast" => {
                    multicast_address = Some(value.parse().map_err(|_| error_message).unwrap())
                }
                _ => panic!("unexpect prompt: {}", prompt),
            }
        }
        Ok(Self {
            replica_address,
            multicast_address,
            n_fault: n_fault.unwrap(),
            signing_key: HashMap::new(), // fill later
        })
    }
}

impl<T: Transport + ?Sized> Config<T> {
    pub fn collect_signing_key(&mut self, path: &Path) {
        for (i, replica) in self.replica_address.iter().enumerate() {
            let prefix = path.file_name().unwrap().to_str().unwrap();
            let key = path.with_file_name(format!("{}-{}.pem", prefix, i));
            let key = {
                let mut buf = String::new();
                File::open(key).unwrap().read_to_string(&mut buf).unwrap();
                buf
            };
            let key = key.parse().unwrap();
            self.signing_key.insert(replica.clone(), key);
        }
    }
}
