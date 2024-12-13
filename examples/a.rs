use std::{
    thread,
    time::Duration,
};

use unsquashfs_wrapper::Unsquashfs;

fn main() {
    let unsquashfs = Unsquashfs::new();
    let unsquashfs_clone = unsquashfs.clone();
    thread::spawn(move || {
        unsquashfs.extract(
            "/home/saki/aosc-os_base_20240215_amd64.squashfs",
            "/test",
            None,
            move |c| {
                dbg!(c);
            },
        )
        .unwrap();
    });

    thread::sleep(Duration::from_secs(10));
    unsquashfs_clone.cancel().unwrap();
}
