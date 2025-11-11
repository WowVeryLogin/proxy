use lazy_static::lazy_static;
use regex::Regex;

pub const LISTEN_ADDR: &str = "127.0.0.1:8080";
pub const MAX_CONNECT_LINE_LEN: usize = 512;
pub const CONNECT_BUFFER_SIZE: usize = 1024;
pub const COPY_BUFFER_SIZE: usize = 8192;

pub const MAX_RETRY_ATTEMPTS: u32 = 3;
pub const RETRY_DELAY_MS: u64 = 500;

lazy_static! {
    pub static ref CONNECT_REGEX: Regex = Regex::new("CONNECT (.+) HTTP/1.1\\r\\n").unwrap();
}

pub const INITIAL_READ_TIMEOUT_SEC: u64 = 1;
pub const INITIAL_CONNECT_TIMEOUT_SEC: u64 = 1;
pub const PROXY_READ_TIMEOUT_SEC: u64 = 30;
pub const PROXY_WRITE_TIMEOUT_SEC: u64 = 30;
