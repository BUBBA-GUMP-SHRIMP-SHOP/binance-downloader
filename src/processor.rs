use crossbeam::channel::Receiver;
use parking_lot::RwLock;

pub trait Processor<'a> {
    type In;
    type Out;
    async fn process(&self, value: &Self::In) -> anyhow::Result<()>;
    fn connect(&self, iter: Receiver<Self::In>);
}

pub trait ProcessConnector<'a> {
    type Out;
    fn connect_to(&self, processor: impl Processor<'a, In = Self::Out>);
}

pub struct ProcessorUnit<F, In, Out> {
    func: F,
    _phantom_in: std::marker::PhantomData<In>,

    pub set: std::sync::Arc<RwLock<Vec<Receiver<In>>>>,

    sender: crossbeam::channel::Sender<Out>,
    pub receiver: crossbeam::channel::Receiver<Out>,
}

impl<F, In, Out> ProcessorUnit<F, In, Out>
where
    F: Fn(&In) -> Out + Send + Sync + 'static,
    Out: Send + Sync + 'static,
{
    pub fn new(func: F) -> Self {
        let (sender, receiver) = crossbeam::channel::bounded(10);
        Self {
            func,
            _phantom_in: std::marker::PhantomData,

            set: std::sync::Arc::new(RwLock::new(Vec::new())),

            sender,
            receiver,
        }
    }
}
impl<'a, F, In, Out> Processor<'a> for ProcessorUnit<F, In, Out>
where
    F: Fn(&In) -> Out + Send + Sync + 'static,
    Out: Send + Sync + 'static,
{
    type In = In;
    type Out = Out;

    async fn process(&self, value: &Self::In) -> anyhow::Result<()> {
        let result = (self.func)(value);
        self.sender.send(result)?;
        Ok(())
    }

    fn connect(&self, rcv: Receiver<In>) {
        self.set.write().push(rcv);
    }
}
impl<'a, F, In, Out> ProcessConnector<'a> for ProcessorUnit<F, In, Out>
where
    F: Fn(&In) -> Out + Send + Sync + 'static,
    Out: Send + Sync + 'static,
{
    type Out = Out;

    fn connect_to(&self, processor: impl Processor<'a, In = Self::Out>) {
        processor.connect(self.receiver.clone());
        // Note: This approach won't work with the current design.
        // Consider redesigning to store items directly instead of iterators.
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcessConnector, Processor, ProcessorUnit};
    use crossbeam::channel::{bounded, Receiver};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct SpyProcessor {
        connect_calls: Arc<AtomicUsize>,
    }

    impl<'a> Processor<'a> for SpyProcessor {
        type In = i32;
        type Out = i32;

        async fn process(&self, _value: &Self::In) -> anyhow::Result<()> {
            Ok(())
        }

        fn connect(&self, _iter: Receiver<Self::In>) {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn process_calls_inner_function() {
        let unit = ProcessorUnit::<_, i32, i32>::new(|value| value + 10);
        let result = unit.process(&5).await;
        assert!(result.is_ok());
    }

    #[test]
    fn connect_registers_receiver() {
        let unit = ProcessorUnit::<_, i32, i32>::new(|value| value + 1);
        let (_sender, receiver) = bounded::<i32>(1);

        unit.connect(receiver);

        assert_eq!(unit.set.read().len(), 1);
    }

    #[test]
    fn connect_to_invokes_downstream_connect() {
        let source = ProcessorUnit::<_, i32, i32>::new(|value| value + 1);
        let calls = Arc::new(AtomicUsize::new(0));
        let downstream = SpyProcessor {
            connect_calls: Arc::clone(&calls),
        };

        source.connect_to(downstream);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
