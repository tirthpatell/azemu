//! SASL frame bodies (part 5, section 5.3).

use crate::codec::DecodeError;
use crate::fields::{described, opt, symbols, Fields};
use crate::types::Value;

pub const DESC_SASL_MECHANISMS: u64 = 0x40;
pub const DESC_SASL_INIT: u64 = 0x41;
pub const DESC_SASL_CHALLENGE: u64 = 0x42;
pub const DESC_SASL_RESPONSE: u64 = 0x43;
pub const DESC_SASL_OUTCOME: u64 = 0x44;

/// Outcome codes of `sasl-outcome`.
pub mod outcome {
    pub const OK: u8 = 0;
    pub const AUTH: u8 = 1;
    pub const SYS: u8 = 2;
    pub const SYS_PERM: u8 = 3;
    pub const SYS_TEMP: u8 = 4;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sasl {
    Mechanisms { mechanisms: Vec<String> },
    Init { mechanism: String, initial_response: Option<Vec<u8>>, hostname: Option<String> },
    Challenge { challenge: Vec<u8> },
    Response { response: Vec<u8> },
    Outcome { code: u8, additional_data: Option<Vec<u8>> },
}

impl Sasl {
    pub fn to_value(&self) -> Value {
        match self {
            Sasl::Mechanisms { mechanisms } => described(DESC_SASL_MECHANISMS, vec![symbols(mechanisms)]),
            Sasl::Init { mechanism, initial_response, hostname } => described(
                DESC_SASL_INIT,
                vec![
                    Value::Symbol(mechanism.clone()),
                    initial_response.clone().map(Value::Binary).unwrap_or(Value::Null),
                    opt(hostname.clone()),
                ],
            ),
            Sasl::Challenge { challenge } => described(DESC_SASL_CHALLENGE, vec![Value::Binary(challenge.clone())]),
            Sasl::Response { response } => described(DESC_SASL_RESPONSE, vec![Value::Binary(response.clone())]),
            Sasl::Outcome { code, additional_data } => {
                described(DESC_SASL_OUTCOME, vec![Value::Ubyte(*code), additional_data.clone().map(Value::Binary).unwrap_or(Value::Null)])
            }
        }
    }

    pub fn from_value(v: &Value) -> Result<Sasl, DecodeError> {
        match v.descriptor_code() {
            Some(DESC_SASL_MECHANISMS) => {
                let f = Fields::of(v, DESC_SASL_MECHANISMS, "amqp:sasl-mechanisms:list")?;
                Ok(Sasl::Mechanisms { mechanisms: f.symbols(0) })
            }
            Some(DESC_SASL_INIT) => {
                let f = Fields::of(v, DESC_SASL_INIT, "amqp:sasl-init:list")?;
                Ok(Sasl::Init { mechanism: f.req_string(0)?, initial_response: f.opt_binary(1)?, hostname: f.opt_string(2)? })
            }
            Some(DESC_SASL_CHALLENGE) => {
                let f = Fields::of(v, DESC_SASL_CHALLENGE, "amqp:sasl-challenge:list")?;
                Ok(Sasl::Challenge { challenge: f.opt_binary(0)?.unwrap_or_default() })
            }
            Some(DESC_SASL_RESPONSE) => {
                let f = Fields::of(v, DESC_SASL_RESPONSE, "amqp:sasl-response:list")?;
                Ok(Sasl::Response { response: f.opt_binary(0)?.unwrap_or_default() })
            }
            Some(DESC_SASL_OUTCOME) => {
                let f = Fields::of(v, DESC_SASL_OUTCOME, "amqp:sasl-outcome:list")?;
                Ok(Sasl::Outcome { code: f.opt_u8(0)?.unwrap_or(0), additional_data: f.opt_binary(1)? })
            }
            _ => Err(DecodeError::Invalid("unknown SASL frame body")),
        }
    }
}
