use crate::generated::schema::WindowAppConnectInfo;

pub struct IpInfo {
    data: (Vec<u8>, u8, String),
    pub server_ip: u8,
    pub server_port: String,
}

impl IpInfo {
    pub fn new(data: (Vec<u8>, u8, String), port: &str) -> Self {
        IpInfo {
            data,
            server_ip: 1_u8,
            server_port: port.into(),
        }
    }
    pub fn set_server_ip(&mut self, server_ip: u8) {
        self.server_ip = server_ip;
    }
    pub fn get_data(&self) -> (Vec<u8>, u8, String) {
        self.data.clone()
    }

    pub fn get_full_server_ip(&self) -> String {
        format!(
            "ws://{}.{}:{}",
            self.data.2.as_str(),
            self.server_ip,
            self.server_port
        )
    }
}

pub trait GetIp {
    fn get_ip_range(&self) -> IpInfo;
}

impl GetIp for WindowAppConnectInfo<'_> {
    fn get_ip_range(&self) -> IpInfo {
        IpInfo::new(
            (
                vec![get_ip(&self.current_ip)],
                get_ip(&self.broadcast_ip),
                get_base_ip(&self.current_ip),
            ),
            &self.port,
        )
    }
}

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

fn get_base_ip(ip: &str) -> String {
    let mut base_ip = ip.split('.').collect::<Vec<&str>>();
    base_ip.pop();
    base_ip.join(".")
}
