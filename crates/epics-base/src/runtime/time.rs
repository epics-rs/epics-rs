use std::time::{Duration, Instant, SystemTime};

pub fn now_wall() -> SystemTime {
    SystemTime::now()
}

pub fn now_mono() -> Instant {
    Instant::now()
}

pub fn deadline_from_now(d: Duration) -> Instant {
    Instant::now() + d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_wall() {
        let t = now_wall();
        assert!(t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() > 0);
    }

    #[test]
    fn test_now_mono() {
        let t1 = now_mono();
        let t2 = now_mono();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_deadline_from_now() {
        let before = Instant::now();
        let deadline = deadline_from_now(Duration::from_secs(10));
        assert!(deadline > before);
        assert!(deadline <= before + Duration::from_secs(11));
    }
}
