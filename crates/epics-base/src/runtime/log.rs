#[macro_export]
macro_rules! rt_debug {
    ($($arg:tt)*) => {
        eprintln!("[DEBUG] {}", format!($($arg)*));
    };
}

#[macro_export]
macro_rules! rt_info {
    ($($arg:tt)*) => {
        eprintln!("[INFO] {}", format!($($arg)*));
    };
}

#[macro_export]
macro_rules! rt_warn {
    ($($arg:tt)*) => {
        eprintln!("[WARN] {}", format!($($arg)*));
    };
}

#[macro_export]
macro_rules! rt_error {
    ($($arg:tt)*) => {
        eprintln!("[ERROR] {}", format!($($arg)*));
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_log_macros_compile() {
        rt_debug!("debug message {}", 42);
        rt_info!("info message");
        rt_warn!("warn: {}", "something");
        rt_error!("error: {} {}", "bad", "thing");
    }
}
