use std::{error::Error, fmt, path::Path};

use crate::model::{
    ALLOWED_FLAGS, ATTACHED_OPTION_PREFIXES, OPTIONS_WITH_VALUES, is_safe_openssh_option,
    is_valid_host_target,
};

#[derive(Debug, PartialEq)]
pub struct ImportedTunnel {
    pub local_port: u16,
    pub destination_host: String,
    pub destination_port: u16,
    pub ssh_host: String,
    pub bind_address: Option<String>,
    pub additional_arguments: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum ParseError {
    Empty,
    NotSsh,
    UnclosedQuote,
    MissingForward,
    InvalidForward,
    MissingHost,
    MissingOptionValue(String),
    UnsupportedOption(String),
    UnsafeOption(String),
    RemoteCommand,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "Paste an SSH command first."),
            Self::NotSsh => write!(formatter, "The command needs to start with ssh."),
            Self::UnclosedQuote => {
                write!(formatter, "One of the quotes in the command is not closed.")
            }
            Self::MissingForward => write!(formatter, "The command needs one -L forward."),
            Self::InvalidForward => write!(formatter, "Use -L localPort:host:remotePort."),
            Self::MissingHost => write!(formatter, "The SSH host is missing."),
            Self::MissingOptionValue(option) => write!(formatter, "{option} needs a value."),
            Self::UnsupportedOption(option) => {
                write!(
                    formatter,
                    "{option} is not supported by the quick importer."
                )
            }
            Self::UnsafeOption(option) => write!(
                formatter,
                "{option} is blocked because it can execute commands or access arbitrary files."
            ),
            Self::RemoteCommand => {
                write!(
                    formatter,
                    "RelayBar only imports forwarding commands, not remote commands."
                )
            }
        }
    }
}

impl Error for ParseError {}

pub fn parse(command: &str) -> Result<ImportedTunnel, ParseError> {
    let tokens = tokenize(command)?;
    let Some(executable) = tokens.first() else {
        return Err(ParseError::Empty);
    };
    if Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("ssh")
    {
        return Err(ParseError::NotSsh);
    }

    let mut forward = None;
    let mut ssh_host = None;
    let mut additional_arguments = Vec::new();
    let mut index = 1;

    while index < tokens.len() {
        let token = &tokens[index];
        if ssh_host.is_some() {
            return Err(ParseError::RemoteCommand);
        }

        if token == "--" {
            index += 1;
            ssh_host = tokens.get(index).cloned();
            if ssh_host.is_none() {
                return Err(ParseError::MissingHost);
            }
        } else if token == "-L" {
            index += 1;
            let value = tokens
                .get(index)
                .ok_or_else(|| ParseError::MissingOptionValue("-L".into()))?;
            if forward.replace(value.clone()).is_some() {
                return Err(ParseError::UnsupportedOption("Multiple -L forwards".into()));
            }
        } else if let Some(value) = token.strip_prefix("-L").filter(|value| !value.is_empty()) {
            if forward.replace(value.to_owned()).is_some() {
                return Err(ParseError::UnsupportedOption("Multiple -L forwards".into()));
            }
        } else if ["-N", "-T", "-n", "-f"].contains(&token.as_str()) {
            // RelayBar owns these process-management flags.
        } else if ALLOWED_FLAGS.contains(&token.as_str()) {
            additional_arguments.push(token.clone());
        } else if OPTIONS_WITH_VALUES.contains(&token.as_str()) {
            index += 1;
            let value = tokens
                .get(index)
                .ok_or_else(|| ParseError::MissingOptionValue(token.clone()))?;
            if token == "-o" && !is_safe_openssh_option(value) {
                return Err(ParseError::UnsafeOption(format!("-o {value}")));
            }
            additional_arguments.extend([token.clone(), value.clone()]);
        } else if token.starts_with('-') {
            let Some(prefix) = ATTACHED_OPTION_PREFIXES
                .iter()
                .find(|prefix| token.starts_with(**prefix) && token.len() > prefix.len())
            else {
                return Err(ParseError::UnsupportedOption(token.clone()));
            };
            if *prefix == "-o" && !is_safe_openssh_option(&token[2..]) {
                return Err(ParseError::UnsafeOption(token.clone()));
            }
            additional_arguments.push(token.clone());
        } else {
            ssh_host = Some(token.clone());
        }
        index += 1;
    }

    let forward = forward.ok_or(ParseError::MissingForward)?;
    let ssh_host = ssh_host
        .filter(|host| is_valid_host_target(host))
        .ok_or(ParseError::MissingHost)?;
    let (local_port, destination_host, destination_port, bind_address) = parse_forward(&forward)?;

    Ok(ImportedTunnel {
        local_port,
        destination_host,
        destination_port,
        ssh_host,
        bind_address,
        additional_arguments,
    })
}

fn parse_forward(specification: &str) -> Result<(u16, String, u16, Option<String>), ParseError> {
    let (before_destination_port, destination_port) = specification
        .rsplit_once(':')
        .ok_or(ParseError::InvalidForward)?;
    let destination_port = parse_port(destination_port)?;

    let (before_host, destination_host) = if before_destination_port.ends_with(']') {
        let opening_bracket = before_destination_port
            .rfind('[')
            .ok_or(ParseError::InvalidForward)?;
        let separator = opening_bracket
            .checked_sub(1)
            .ok_or(ParseError::InvalidForward)?;
        if before_destination_port.as_bytes()[separator] != b':' {
            return Err(ParseError::InvalidForward);
        }
        (
            &before_destination_port[..separator],
            &before_destination_port[opening_bracket + 1..before_destination_port.len() - 1],
        )
    } else {
        before_destination_port
            .rsplit_once(':')
            .ok_or(ParseError::InvalidForward)?
    };
    if destination_host.is_empty() {
        return Err(ParseError::InvalidForward);
    }

    let (bind_address, local_port) = match before_host.rsplit_once(':') {
        Some((bind_address, local_port)) if !bind_address.is_empty() => {
            (Some(bind_address.to_owned()), parse_port(local_port)?)
        }
        Some(_) => return Err(ParseError::InvalidForward),
        None => (None, parse_port(before_host)?),
    };

    Ok((
        local_port,
        destination_host.to_owned(),
        destination_port,
        bind_address,
    ))
}

fn parse_port(value: &str) -> Result<u16, ParseError> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or(ParseError::InvalidForward)
}

fn tokenize(command: &str) -> Result<Vec<String>, ParseError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaping = false;
    let mut token_started = false;

    for character in command.trim().chars() {
        if escaping {
            current.push(character);
            token_started = true;
            escaping = false;
        } else if character == '\\' && quote != Some('\'') {
            escaping = true;
            token_started = true;
        } else if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                current.push(character);
            }
            token_started = true;
        } else if matches!(character, '\"' | '\'') {
            quote = Some(character);
            token_started = true;
        } else if character.is_whitespace() {
            if token_started {
                tokens.push(std::mem::take(&mut current));
                token_started = false;
            }
        } else {
            current.push(character);
            token_started = true;
        }
    }

    if quote.is_some() {
        return Err(ParseError::UnclosedQuote);
    }
    if escaping {
        current.push('\\');
    }
    if token_started {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_forward() {
        let tunnel = parse("ssh -N -L 8080:localhost:3000 user@example.com").unwrap();
        assert_eq!(tunnel.local_port, 8080);
        assert_eq!(tunnel.destination_host, "localhost");
        assert_eq!(tunnel.destination_port, 3000);
        assert_eq!(tunnel.ssh_host, "user@example.com");
        assert_eq!(tunnel.bind_address, None);
    }

    #[test]
    fn preserves_bind_ipv6_and_safe_options() {
        let tunnel =
            parse("ssh -p 2222 -o 'ConnectTimeout=5' -L0.0.0.0:5432:[::1]:5432 ops@bastion")
                .unwrap();
        assert_eq!(tunnel.bind_address.as_deref(), Some("0.0.0.0"));
        assert_eq!(tunnel.destination_host, "::1");
        assert_eq!(
            tunnel.additional_arguments,
            ["-p", "2222", "-o", "ConnectTimeout=5"]
        );
    }

    #[test]
    fn rejects_commands_and_unsafe_options() {
        assert_eq!(
            parse("ssh -L 8080:localhost:80 host uptime").unwrap_err(),
            ParseError::RemoteCommand
        );
        assert_eq!(
            parse("ssh -o 'ProxyCommand=whoami' -L 8080:localhost:80 host").unwrap_err(),
            ParseError::UnsafeOption("-o ProxyCommand=whoami".into())
        );
        assert_eq!(
            parse("ssh -F /tmp/config -L 8080:localhost:80 host").unwrap_err(),
            ParseError::UnsupportedOption("-F".into())
        );
    }

    #[test]
    fn rejects_invalid_input() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("scp file host:"), Err(ParseError::NotSsh));
        assert_eq!(
            parse("ssh -L '8080:host:80 host"),
            Err(ParseError::UnclosedQuote)
        );
        assert_eq!(parse("ssh host"), Err(ParseError::MissingForward));
        assert_eq!(
            parse("ssh -L 0:host:80 host"),
            Err(ParseError::InvalidForward)
        );
    }
}
