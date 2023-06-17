fn main() {
    distinst_squashfs::extract("/home/saki/aosc-os_base_20230322_amd64.squashfs", "/mnt", move |c| {
        dbg!(c);
    }).unwrap();
}