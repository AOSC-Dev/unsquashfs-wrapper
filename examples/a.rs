use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

fn main() {
    let cancel = Arc::new(AtomicBool::from(false));
    let cc = cancel.clone();
    thread::spawn(move || {
        unsquashfs_wrapper::extract(
            "/home/saki/aosc-os_base_20240215_amd64.squashfs",
            "/test",
            None,
            move |c| {
                dbg!(c);
            },
            cc,
        )
        .unwrap();
    });

    thread::sleep(Duration::from_secs(10));
    cancel.store(true, Ordering::Relaxed);
}
