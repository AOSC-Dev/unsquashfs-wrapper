use std::{thread, time::Duration};

use unsquashfs_wrapper::Unsquashfs;

fn main() {
    let unsquashfs = Unsquashfs::new();
    let uc = unsquashfs.clone();

    let t = thread::spawn(move || {
        unsquashfs
        .extract(
            "/home/saki/aosc-os_base_20240916_amd64.squashfs",
            "/test",
            None,
            Box::new(move |c| {
                dbg!(c);
            }),
        )
    });

    thread::sleep(Duration::from_secs(1));
    uc.cancel().unwrap();

    t.join().unwrap().unwrap();
}
