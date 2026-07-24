use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ALLOWED_FLAGS: &[&str] = &["-4", "-6", "-a", "-C", "-k", "-q", "-v", "-vv", "-vvv"];
pub const OPTIONS_WITH_VALUES: &[&str] = &["-J", "-i", "-l", "-o", "-p"];
pub const ATTACHED_OPTION_PREFIXES: &[&str] = &["-J", "-i", "-l", "-o", "-p"];

const ALLOWED_OPENSSH_OPTIONS: &[&str] = &[
    "addressfamily",
    "batchmode",
    "compression",
    "connectionattempts",
    "connecttimeout",
    "hostkeyalgorithms",
    "identitiesonly",
    "ipqos",
    "kexalgorithms",
    "loglevel",
    "macs",
    "passwordauthentication",
    "port",
    "preferredauthentications",
    "proxyjump",
    "pubkeyauthentication",
    "serveralivecountmax",
    "serveraliveinterval",
    "stricthostkeychecking",
    "tcpkeepalive",
    "user",
    "verifyhostkeydns",
];

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Tunnel {
    pub id: Uuid,
    pub name: String,
    pub local_port: u16,
    pub destination_host: String,
    pub destination_port: u16,
    pub ssh_host: String,
    pub bind_address: Option<String>,
    #[serde(default)]
    pub additional_arguments: Vec<String>,
}

impl Tunnel {
    pub fn new(
        name: String,
        local_port: u16,
        destination_host: String,
        destination_port: u16,
        ssh_host: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            local_port,
            destination_host,
            destination_port,
            ssh_host,
            bind_address: None,
            additional_arguments: Vec::new(),
        }
    }

    pub fn display_name(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() {
            self.destination_endpoint()
        } else {
            name.to_owned()
        }
    }

    pub fn forward_spec(&self) -> String {
        let local = self.bind_address.as_ref().map_or_else(
            || self.local_port.to_string(),
            |host| format!("{host}:{}", self.local_port),
        );
        let destination =
            if self.destination_host.contains(':') && !self.destination_host.starts_with('[') {
                format!("[{}]", self.destination_host)
            } else {
                self.destination_host.clone()
            };
        format!("{local}:{destination}:{}", self.destination_port)
    }

    pub fn local_endpoint(&self) -> String {
        let host = self
            .bind_address
            .as_deref()
            .filter(|host| !host.is_empty())
            .unwrap_or("localhost");
        format!("{host}:{}", self.local_port)
    }

    pub fn destination_endpoint(&self) -> String {
        format!("{}:{}", self.destination_host, self.destination_port)
    }

    pub fn browser_url(&self) -> String {
        let mut host = self.bind_address.as_deref().unwrap_or("").trim();
        if let Some(unwrapped) = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
        {
            host = unwrapped;
        }
        if host.is_empty() || ["*", "0.0.0.0", "::"].contains(&host.to_ascii_lowercase().as_str()) {
            host = "localhost";
        }
        if host.contains(':') {
            format!("http://[{host}]:{}/", self.local_port)
        } else {
            format!("http://{host}:{}/", self.local_port)
        }
    }

    pub fn exposes_beyond_loopback(&self) -> bool {
        let Some(host) = self.bind_address.as_deref() else {
            return false;
        };
        let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
        !["localhost", "127.0.0.1", "::1"].contains(&host.as_str())
    }

    pub fn is_safe_to_run(&self) -> bool {
        is_valid_host_target(&self.ssh_host)
            && is_valid_destination_host(&self.destination_host)
            && are_additional_arguments_safe(&self.additional_arguments)
    }

    pub fn ssh_arguments(&self) -> Vec<String> {
        let mut arguments = vec![
            "-N".into(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
            "-o".into(),
            "ServerAliveInterval=30".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
            "-L".into(),
            self.forward_spec(),
        ];
        arguments.extend(self.additional_arguments.clone());
        arguments.push(self.ssh_host.clone());
        arguments
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum TunnelPhase {
    #[default]
    Stopped,
    Starting,
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_seconds: u64,
        message: String,
    },
    Running,
    Failed(String),
}

impl TunnelPhase {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Retrying { .. } | Self::Running)
    }
}

pub fn is_valid_host_target(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value.starts_with('-')
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

pub fn is_valid_destination_host(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

pub fn is_safe_openssh_option(value: &str) -> bool {
    let value = value.trim();
    let key = value
        .split(['=', ' ', '\t', '\r', '\n'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    !key.is_empty() && ALLOWED_OPENSSH_OPTIONS.contains(&key.as_str())
}

pub fn are_additional_arguments_safe(arguments: &[String]) -> bool {
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if ALLOWED_FLAGS.contains(&argument.as_str()) {
            index += 1;
            continue;
        }
        if OPTIONS_WITH_VALUES.contains(&argument.as_str()) {
            index += 1;
            let Some(value) = arguments.get(index) else {
                return false;
            };
            if argument == "-o" && !is_safe_openssh_option(value) {
                return false;
            }
            index += 1;
            continue;
        }
        let Some(prefix) = ATTACHED_OPTION_PREFIXES
            .iter()
            .find(|prefix| argument.starts_with(**prefix) && argument.len() > prefix.len())
        else {
            return false;
        };
        if *prefix == "-o" && !is_safe_openssh_option(&argument[2..]) {
            return false;
        }
        index += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tunnel(bind_address: Option<&str>) -> Tunnel {
        let mut tunnel = Tunnel::new("Web".into(), 8080, "::1".into(), 3000, "example.com".into());
        tunnel.bind_address = bind_address.map(str::to_owned);
        tunnel
    }

    #[test]
    fn formats_endpoints_and_browser_urls() {
        assert_eq!(tunnel(None).forward_spec(), "8080:[::1]:3000");
        assert_eq!(tunnel(None).browser_url(), "http://localhost:8080/");
        assert_eq!(
            tunnel(Some("0.0.0.0")).browser_url(),
            "http://localhost:8080/"
        );
        assert_eq!(tunnel(Some("[::1]")).browser_url(), "http://[::1]:8080/");
    }

    #[test]
    fn rejects_unsafe_arguments() {
        assert!(!is_valid_host_target("-oProxyCommand=whoami"));
        assert!(!are_additional_arguments_safe(&[
            "-o".into(),
            "LocalCommand=whoami".into()
        ]));
        assert!(are_additional_arguments_safe(&[
            "-p".into(),
            "2222".into(),
            "-oIdentitiesOnly=yes".into(),
        ]));
    }
}
