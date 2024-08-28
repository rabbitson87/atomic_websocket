use chrono::{DateTime, FixedOffset, Local, Utc};

pub fn now() -> DateTime<FixedOffset> {
    let now = Utc::now();
    let offset_in_sec = Local::now().offset().local_minus_utc();
    let offset = FixedOffset::east_opt(offset_in_sec).unwrap();
    now.with_timezone(&offset)
}

#[test]
fn test_now() {
    let now = now();
    println!("{:?}", now);
}
