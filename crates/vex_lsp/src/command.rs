//! Opaque server commands. Arguments are protocol data, never shell input.

use serde_json::{Value, json};
use std::{io, sync::Arc};

#[derive(Clone, Debug)]
pub struct ServerCommand {
    pub name: Arc<str>,
    pub arguments: Arc<[Value]>,
}

impl ServerCommand {
    pub(crate) fn params(&self, capabilities: &Value) -> Result<Value, String> {
        self.validate(capabilities)?;
        Ok(json!({"command":self.name.as_ref(),"arguments":self.arguments.as_ref()}))
    }

    pub(crate) fn validate(&self, capabilities: &Value) -> Result<(), String> {
        if self.name.is_empty() || self.name.len() > 4096 || self.name.chars().any(char::is_control)
        {
            return Err("invalid language server command name".into());
        }
        if !capabilities["executeCommandProvider"]["commands"]
            .as_array()
            .is_some_and(|commands| {
                commands
                    .iter()
                    .any(|command| command.as_str() == Some(&self.name))
            })
        {
            return Err(format!(
                "language server does not advertise command {}",
                self.name
            ));
        }
        // Check a borrowed payload before cloning it into the outgoing message.
        // Stop at the limit without allocating another large byte buffer.
        #[derive(serde::Serialize)]
        struct Params<'a> {
            command: &'a str,
            arguments: &'a [Value],
        }
        serde_json::to_writer(
            Budget(8 << 20),
            &Params {
                command: &self.name,
                arguments: &self.arguments,
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    }
}

struct Budget(usize);
impl io::Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("language server command exceeds 8 MiB"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_advertised_bounded_commands_can_be_sent() {
        let capabilities = json!({"executeCommandProvider":{"commands":["fix"]}});
        let mut command = ServerCommand {
            name: "fix".into(),
            arguments: Arc::from([json!({"data":1})]),
        };
        assert_eq!(
            command.params(&capabilities).unwrap(),
            json!({"command":"fix","arguments":[{"data":1}]})
        );
        command.name = "unknown".into();
        assert!(
            command
                .params(&capabilities)
                .unwrap_err()
                .contains("does not advertise")
        );
        command.name = "fix\n".into();
        assert!(
            command
                .params(&capabilities)
                .unwrap_err()
                .contains("invalid")
        );
        command.name = "fix".into();
        command.arguments = Arc::from([json!("x".repeat(8 << 20))]);
        assert!(command.params(&capabilities).unwrap_err().contains("8 MiB"));
    }
}
