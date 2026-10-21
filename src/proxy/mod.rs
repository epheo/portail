//! The data plane: listener management, connection workers, HTTP
//! parsing/filtering, backend connection pooling, TLS, and backend health.
//! Everything that touches a client or backend socket lives here; the
//! control planes (config file, kubernetes/) only hand it route tables.

pub mod backend_pool;
pub mod chunked;
pub mod data_plane;
pub(crate) mod h2_bridge;
pub mod health;
pub mod http_filters;
pub mod http_parser;
pub mod method;
pub mod request_processor;
pub mod tls;
pub mod udp_worker;
pub mod worker;

/// Test-only helpers for running the proxy out of fds.
#[cfg(test)]
pub(crate) mod test_util {
    /// RLIMIT_NOFILE lowered to the lowest free fd, so the next socket()
    /// fails with EMFILE; the old limit comes back on drop.
    pub(crate) struct FdsExhausted(libc::rlimit);

    impl FdsExhausted {
        pub(crate) fn new() -> Self {
            let mut old = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut old) }, 0);
            let lowest_free = unsafe { libc::dup(2) };
            assert!(lowest_free >= 0);
            unsafe { libc::close(lowest_free) };
            let low = libc::rlimit {
                rlim_cur: lowest_free as libc::rlim_t,
                rlim_max: old.rlim_max,
            };
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) }, 0);
            Self(old)
        }
    }

    impl Drop for FdsExhausted {
        fn drop(&mut self) {
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
        }
    }

    /// Whether the test `name` in `module` runs in a process of its own,
    /// as a lowered fd limit would starve every test running beside it.
    /// The parent runs it in a child process, asserts it passed, and gets
    /// false.
    pub(crate) fn in_own_process(module: &str, name: &str) -> bool {
        const CHILD: &str = "PORTAIL_TEST_OWN_PROCESS";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let test = format!("{}::{}", module.split_once("::").unwrap().1, name);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([&test, "--exact", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && log.contains("1 passed"), "{log}");
        false
    }
}
