use super::*;

#[test]
fn reads_df() {
    let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1 971350180 500000000 400000000 56% /\n";
    assert_eq!(parse_df(out), Some((400000000 * 1024, 971350180 * 1024)));
    assert_eq!(parse_df("header only\n"), None);
}

#[cfg(not(windows))]
#[test]
fn app_paths_are_the_computers() {
    assert_eq!(native("/a/b"), PathBuf::from("/a/b"));
    assert_eq!(app_path(Path::new("/a/b")), "/a/b");
}

#[test]
fn finds_the_file_system_holding_a_folder() {
    let mount = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                     /dev/disk4s1 on /Volumes/STICK (msdos, local, nodev, nosuid, noowners)\n\
                     /dev/disk5s1 on /Volumes/STICK 2 (exfat, local, nodev, nosuid, noowners)\n";
    assert_eq!(
        fs_in_mount(mount, "/Volumes/STICK/DCIM").as_deref(),
        Some("msdos")
    );
    assert_eq!(
        fs_in_mount(mount, "/Volumes/STICK").as_deref(),
        Some("msdos")
    );
    assert_eq!(
        fs_in_mount(mount, "/Volumes/STICK 2/a").as_deref(),
        Some("exfat")
    );
    assert_eq!(
        fs_in_mount(mount, "/Volumes/STICKS").as_deref(),
        Some("apfs")
    );
    assert_eq!(fs_in_mount(mount, "/Users/me").as_deref(), Some("apfs"));
}

#[cfg(windows)]
#[test]
fn drives_are_top_folders() {
    assert_eq!(native("/C:/Users/me"), PathBuf::from(r"C:\Users\me"));
    assert_eq!(native("/C:"), PathBuf::from(r"C:\"));
    assert_eq!(app_path(Path::new(r"C:\Users\me")), "/C:/Users/me");
    assert_eq!(app_path(Path::new(r"\\?\D:\x\")), "/D:/x");
}

#[cfg(windows)]
#[test]
fn network_shares_keep_their_server() {
    assert_eq!(
        app_path(Path::new(r"\\nas\Photos\a.jpg")),
        "//nas/Photos/a.jpg"
    );
    assert_eq!(app_path(Path::new(r"\\?\UNC\nas\Photos\")), "//nas/Photos");
    assert_eq!(
        native("//nas/Photos/a.jpg"),
        PathBuf::from(r"\\nas\Photos\a.jpg")
    );
    assert_eq!(native("/C:/x"), PathBuf::from(r"C:\x"));
}

#[test]
fn reads_network_mounts() {
    let mount = "/dev/disk3s1s1 on / (apfs, sealed, local)\n//me@nas._smb._tcp.local/Photos on /Volumes/Photos 1 (smbfs, nodev, nosuid, mounted by me)\n\
                     nas:/export/media/ on /Volumes/media (nfs, nodev, nosuid, mounted by me)\n\
                     https://cloud.example.com/dav/ on /Volumes/dav (webdav, nodev, noexec, nosuid, mounted by me)\n\
                     map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n";
    assert_eq!(
        net_in_mount(mount),
        vec![
            (
                "smb://me@nas._smb._tcp.local/Photos".to_string(),
                "/Volumes/Photos 1".to_string()
            ),
            (
                "nfs://nas/export/media".to_string(),
                "/Volumes/media".to_string()
            ),
            (
                "https://cloud.example.com/dav/".to_string(),
                "/Volumes/dav".to_string()
            ),
        ]
    );
    let net =
            "Status       Local     Remote                    Network\n-----\nOK           Z:        \\\\nas\\Photos              Microsoft Windows Network\n\
                   OK           Y:        \\\\nas\\export\\media        NFS Network\nThe command completed successfully.\n";
    assert_eq!(
        net_in_net_use(net),
        vec![
            ("smb://nas/Photos".to_string(), "/Z:".to_string()),
            ("nfs://nas/export/media".to_string(), "/Y:".to_string()),
        ]
    );
    let proc = "/dev/sdb1 /media/me/STICK vfat rw 0 0\nnas:/export /home/me/nas nfs4 rw 0 0\n//nas/Photos /mnt/photos cifs rw 0 0\nme@box:/srv /home/me/box fuse.sshfs rw 0 0\n";
    assert_eq!(
        net_in_proc(proc),
        vec![
            ("nfs://nas/export".to_string(), "/home/me/nas".to_string()),
            ("smb://nas/Photos".to_string(), "/mnt/photos".to_string()),
            ("sftp://me@box/srv".to_string(), "/home/me/box".to_string()),
        ]
    );
    assert_eq!(
        gvfs_url("smb-share:server=nas,share=photos,user=me").as_deref(),
        Some("smb://me@nas/photos")
    );
    assert_eq!(
        gvfs_url("nfs:host=nas,prefix=%2Fexport%2Fmedia").as_deref(),
        Some("nfs://nas/export/media")
    );
    assert_eq!(
        gvfs_url("dav:host=cloud,ssl=true,prefix=%2Fdav").as_deref(),
        Some("https://cloud/dav")
    );
    assert_eq!(
        gvfs_url("ftp:host=box,port=2121").as_deref(),
        Some("ftp://box:2121")
    );
    assert_eq!(gvfs_url("mtp:host=Phone"), None);
    assert_eq!(applescript_text(r#"a"b\c"#), r#"a\"b\\c"#);
    assert_eq!(
        file_uri(Path::new("/home/me/A b,c/é.txt")),
        "file:///home/me/A%20b%2Cc/%C3%A9.txt"
    );
    assert_eq!(powershell_text("it's"), "it''s");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn reads_mounts() {
    let text = "/dev/sdb1 /media/me/MY\\040STICK vfat rw 0 0\nproc /proc proc rw 0 0\n";
    assert_eq!(
        mounts(text)[0],
        ("/dev/sdb1".to_string(), "/media/me/MY STICK".to_string())
    );
}
