#[cfg(test)]
mod tests {
    use epoch::{DropBox, Registration};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    struct CountDrops {
        count: Arc<AtomicUsize>,
    }

    impl Drop for CountDrops {
        fn drop(&mut self) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn test() {
        let countdrops = Arc::new(AtomicUsize::new(0));
        let dup1 = Box::into_raw(Box::new(CountDrops {
            count: Arc::clone(&countdrops),
        }));
        let atomic = AtomicPtr::new(dup1);
        static DROPBOX: DropBox = DropBox::new();
        std::thread::scope(|s| {
            for _ in 0..15 {
                s.spawn(|| {
                    let dup2 = CountDrops {
                        count: Arc::clone(&countdrops),
                    };
                    let dup3 = CountDrops {
                        count: Arc::clone(&countdrops),
                    };
                    let dup4 = CountDrops {
                        count: Arc::clone(&countdrops),
                    };
                    let worker = Registration::create_register();
                    let res = worker.load(&atomic);
                    std::mem::drop(res);
                    worker.swap(&atomic, dup2, &DROPBOX);
                    worker.swap(&atomic, dup3, &DROPBOX);
                    worker.swap(&atomic, dup4, &DROPBOX);
                });
            }
        });

        // just to check whether things are getting dropped or not!
        println!("{}", countdrops.load(Ordering::Relaxed));
    }
}
