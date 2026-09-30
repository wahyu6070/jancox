//! Parallel work with results in order.

use std::collections::BTreeMap;
use std::io;
use std::sync::{mpsc, Arc, Mutex};

/// Runs `work` on `threads` threads over the jobs `produce` returns (until
/// `None`) and hands the results to `consume` in job order. At most
/// `2 * threads` jobs are in memory at once.
pub fn pipeline<J: Send, O: Send>(
    threads: usize,
    mut produce: impl FnMut() -> io::Result<Option<J>> + Send,
    work: impl Fn(J) -> io::Result<O> + Sync,
    mut consume: impl FnMut(O) -> io::Result<()>,
) -> io::Result<()> {
    let threads = threads.max(1);
    let in_flight = threads * 2;
    let (job_tx, job_rx) = mpsc::sync_channel::<(usize, J)>(threads);
    // shared by the workers; dropped with the last one, which stops the
    // producer when they quit early
    let job_rx = Arc::new(Mutex::new(job_rx));
    let (res_tx, res_rx) = mpsc::channel::<(usize, io::Result<O>)>();
    // the producer takes a credit per job, the consumer gives it back
    let (credit_tx, credit_rx) = mpsc::sync_channel::<()>(in_flight);
    for _ in 0..in_flight {
        credit_tx.send(()).unwrap();
    }
    let work = &work;
    std::thread::scope(|s| -> io::Result<()> {
        let producer = s.spawn(move || -> io::Result<()> {
            for i in 0.. {
                if credit_rx.recv().is_err() {
                    break;
                }
                let Some(job) = produce()? else { break };
                if job_tx.send((i, job)).is_err() {
                    break;
                }
            }
            Ok(())
        });
        for _ in 0..threads {
            let res_tx = res_tx.clone();
            let job_rx = Arc::clone(&job_rx);
            s.spawn(move || loop {
                let job = job_rx.lock().unwrap().recv();
                let Ok((i, job)) = job else { break };
                if res_tx.send((i, work(job))).is_err() {
                    break;
                }
            });
        }
        drop(res_tx);
        drop(job_rx);

        let mut pending = BTreeMap::new();
        let mut next = 0;
        let consumed = (|| -> io::Result<()> {
            // ends when the producer is done and all workers have quit
            while let Ok((i, out)) = res_rx.recv() {
                pending.insert(i, out);
                while let Some(out) = pending.remove(&next) {
                    consume(out?)?;
                    next += 1;
                    let _ = credit_tx.send(());
                }
            }
            Ok(())
        })();
        drop(res_rx);
        drop(credit_tx);
        let produced = producer.join().expect("payload reader panicked");
        // a producer error is the cause of a short result
        produced.and(consumed)?;
        if !pending.is_empty() {
            return Err(io::Error::other("payload pipeline stopped early"));
        }
        Ok(())
    })
}
