//! Servers as drives: SFTP, SMB and NFS, WebDAV, FTP, S3 and Everything, and the passwords kept
//! for them in the system's keychain.

pub(crate) mod everything;
pub(crate) mod ftp;
pub(crate) mod s3;
pub(crate) mod secrets;
pub(crate) mod sftp;
pub(crate) mod smb;
pub(crate) mod webdav;
