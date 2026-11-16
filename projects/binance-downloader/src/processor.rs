use anyhow::{Result, ensure};
use std::future::Future;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt},
    task::JoinSet,
};

/// Process a bounded channel until all senders close, then finish active work.
pub async fn run_channel<F, Fut>(
    mut receiver: tokio::sync::mpsc::Receiver<String>,
    parallelism: usize,
    mut worker: F,
) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    ensure!(parallelism > 0, "parallelism must be positive");
    let mut tasks = JoinSet::new();
    let result: Result<()> = async {
        loop {
            if tasks.len() >= parallelism {
                tasks.join_next().await.transpose()?;
            }
            let Some(line) = receiver.recv().await else {
                break;
            };
            tasks.spawn(worker(line));
        }
        Ok(())
    }
    .await;
    let mut task_result = Ok(());
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            task_result = Err(error.into());
        }
    }
    result?;
    task_result
}

/// Read incrementally and apply backpressure without blocking runtime threads.
/// Item failures are reported by the worker. A line containing `poison pill`
/// ends input; previously started workers are drained normally.
pub async fn run<R, F, Fut>(reader: R, parallelism: usize, mut worker: F) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    F: FnMut(String) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    ensure!(parallelism > 0, "parallelism must be positive");
    let mut tasks = JoinSet::new();
    let mut lines = reader.lines();
    let input_result: Result<()> = async {
        loop {
            if tasks.len() >= parallelism {
                tasks.join_next().await.transpose()?;
            }
            let Some(line) = lines.next_line().await? else {
                break;
            };
            if line.contains("poison pill") {
                break;
            }
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            tasks.spawn(worker(line.to_owned()));
        }
        Ok(())
    }
    .await;
    // Finish active writes even when reading the input fails.
    let mut task_result = Ok(());
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            task_result = Err(error.into());
        }
    }
    input_result?;
    task_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    // A single-thread runtime exposes the old blocking-channel deadlock.
    #[tokio::test(flavor = "current_thread")]
    async fn bounds_concurrency_and_drains_all_work() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let input = format!(
            "# comment\n\n{}",
            (0..100).map(|n| format!("{n}\n")).collect::<String>()
        );
        run(input.as_bytes(), 3, |_| {
            let (active, peak, done) = (active.clone(), peak.clone(), done.clone());
            async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                tokio::task::yield_now().await;
                active.fetch_sub(1, Ordering::SeqCst);
                done.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await
        .unwrap();
        assert_eq!(done.load(Ordering::SeqCst), 100);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn rejects_zero_workers() {
        assert!(run(&b"one\n"[..], 0, |_| async {}).await.is_err());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn poison_pill_stops_input_and_finishes_started_workers() {
        for marker in ["poison pill", "prefix poison pill suffix", "# poison pill"] {
            let done = Arc::new(AtomicUsize::new(0));
            let input = format!("first\nsecond\n{marker}\nmust not run\n");
            let mut dispatched = Vec::new();
            run(input.as_bytes(), 4, |line| {
                dispatched.push(line);
                let done = done.clone();
                async move {
                    tokio::task::yield_now().await;
                    done.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
            .unwrap();
            assert_eq!(dispatched, ["first", "second"]);
            assert_eq!(done.load(Ordering::SeqCst), 2);
        }
    }
}
