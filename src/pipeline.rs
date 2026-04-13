use crossbeam::channel::{Receiver, Sender};

use crate::file_info::FileInfo;

#[derive(Clone)]
pub enum Event<T> {
    Data(T),
    End,
}

#[derive(Clone)]
pub struct Transaction<T> {
    pub tx: Sender<T>,
    pub rx: Receiver<T>,
}
impl<T> From<(Sender<T>, Receiver<T>)> for Transaction<T> {
    fn from(value: (Sender<T>, Receiver<T>)) -> Self {
        Self {
            tx: value.0,
            rx: value.1,
        }
    }
}

#[derive(Clone)]
pub struct Pipeline {
    pub url: Transaction<Event<String>>,
    pub download: Transaction<Event<FileInfo>>,
    pub not_remote: Transaction<Event<FileInfo>>,
}
