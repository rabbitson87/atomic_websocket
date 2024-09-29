pub fn get_ip(ip: &str) -> u8 {
    match ip {
        "" => 0,
        _ => match ip.split('.').collect::<Vec<&str>>().last() {
            Some(ip) => match ip.split(':').next().unwrap().parse::<u8>() {
                Ok(ip) => ip,
                Err(_) => 0,
            },
            None => 0,
        },
    }
}
