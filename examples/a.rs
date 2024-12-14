use unsquashfs_wrapper::Unsquashfs;

fn main() {
    let unsquashfs = Unsquashfs::new();
    unsquashfs
        .extract(
            "/home/saki/aosc-os_base_20240916_amd64.squashfs",
            "/test",
            None,
            Box::new(move |c| {
                dbg!(c);
            }),
        )
        .unwrap();
}
