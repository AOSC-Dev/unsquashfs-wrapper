fn main() {
    distinst_squashfs::extract(
        "/home/saki/aosc-os_base_20230322_amd64.squashfs",
        "/tmp/test",
        None,
        move |c| {
            dbg!(c);
        },
    )
    .unwrap();
}
