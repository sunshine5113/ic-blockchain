// Autogenerated by Thrift Compiler (0.13.0)
// DO NOT EDIT UNLESS YOU ARE SURE THAT YOU KNOW WHAT YOU ARE DOING

#![allow(unused_imports)]
#![allow(unused_extern_crates)]
#![cfg_attr(feature = "cargo-clippy", allow(too_many_arguments, type_complexity))]
#![cfg_attr(rustfmt, rustfmt_skip)]

extern crate thrift;

use thrift::OrderedFloat;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::convert::{From, TryFrom};
use std::default::Default;
use std::error::Error;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::rc::Rc;

use thrift::{ApplicationError, ApplicationErrorKind, ProtocolError, ProtocolErrorKind, TThriftClient};
use thrift::protocol::{TFieldIdentifier, TListIdentifier, TMapIdentifier, TMessageIdentifier, TMessageType, TInputProtocol, TOutputProtocol, TSetIdentifier, TStructIdentifier, TType};
use thrift::protocol::field_id;
use thrift::protocol::verify_expected_message_type;
use thrift::protocol::verify_expected_sequence_number;
use thrift::protocol::verify_expected_service_call;
use thrift::protocol::verify_required_field_exists;
use thrift::server::TProcessor;

//
// Ingress
//

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Ingress {
  pub source: Option<i64>,
  pub receiver: Option<i64>,
  pub method_name: Option<String>,
  pub method_payload: Option<Vec<u8>>,
  pub message_id: Option<i64>,
  pub message_time_ns: Option<i64>,
}

impl Ingress {
  pub fn new<F1, F2, F3, F4, F5, F6>(source: F1, receiver: F2, method_name: F3, method_payload: F4, message_id: F5, message_time_ns: F6) -> Ingress where F1: Into<Option<i64>>, F2: Into<Option<i64>>, F3: Into<Option<String>>, F4: Into<Option<Vec<u8>>>, F5: Into<Option<i64>>, F6: Into<Option<i64>> {
    Ingress {
      source: source.into(),
      receiver: receiver.into(),
      method_name: method_name.into(),
      method_payload: method_payload.into(),
      message_id: message_id.into(),
      message_time_ns: message_time_ns.into(),
    }
  }
  pub fn read_from_in_protocol(i_prot: &mut dyn TInputProtocol) -> thrift::Result<Ingress> {
    i_prot.read_struct_begin()?;
    let mut f_1: Option<i64> = Some(0);
    let mut f_2: Option<i64> = Some(0);
    let mut f_3: Option<String> = Some("".to_owned());
    let mut f_4: Option<Vec<u8>> = Some(Vec::new());
    let mut f_5: Option<i64> = Some(0);
    let mut f_6: Option<i64> = Some(0);
    loop {
      let field_ident = i_prot.read_field_begin()?;
      if field_ident.field_type == TType::Stop {
        break;
      }
      let field_id = field_id(&field_ident)?;
      match field_id {
        1 => {
          let val = i_prot.read_i64()?;
          f_1 = Some(val);
        },
        2 => {
          let val = i_prot.read_i64()?;
          f_2 = Some(val);
        },
        3 => {
          let val = i_prot.read_string()?;
          f_3 = Some(val);
        },
        4 => {
          let val = i_prot.read_bytes()?;
          f_4 = Some(val);
        },
        5 => {
          let val = i_prot.read_i64()?;
          f_5 = Some(val);
        },
        6 => {
          let val = i_prot.read_i64()?;
          f_6 = Some(val);
        },
        _ => {
          i_prot.skip(field_ident.field_type)?;
        },
      };
      i_prot.read_field_end()?;
    }
    i_prot.read_struct_end()?;
    let ret = Ingress {
      source: f_1,
      receiver: f_2,
      method_name: f_3,
      method_payload: f_4,
      message_id: f_5,
      message_time_ns: f_6,
    };
    Ok(ret)
  }
  pub fn write_to_out_protocol(&self, o_prot: &mut dyn TOutputProtocol) -> thrift::Result<()> {
    let struct_ident = TStructIdentifier::new("Ingress");
    o_prot.write_struct_begin(&struct_ident)?;
    if let Some(fld_var) = self.source {
      o_prot.write_field_begin(&TFieldIdentifier::new("source", TType::I64, 1))?;
      o_prot.write_i64(fld_var)?;
      o_prot.write_field_end()?;
      ()
    } else {
      ()
    }
    if let Some(fld_var) = self.receiver {
      o_prot.write_field_begin(&TFieldIdentifier::new("receiver", TType::I64, 2))?;
      o_prot.write_i64(fld_var)?;
      o_prot.write_field_end()?;
      ()
    } else {
      ()
    }
    if let Some(ref fld_var) = self.method_name {
      o_prot.write_field_begin(&TFieldIdentifier::new("method_name", TType::String, 3))?;
      o_prot.write_string(fld_var)?;
      o_prot.write_field_end()?;
      ()
    } else {
      ()
    }
    if let Some(ref fld_var) = self.method_payload {
      o_prot.write_field_begin(&TFieldIdentifier::new("method_payload", TType::String, 4))?;
      o_prot.write_bytes(fld_var)?;
      o_prot.write_field_end()?;
      ()
    } else {
      ()
    }
    if let Some(fld_var) = self.message_id {
      o_prot.write_field_begin(&TFieldIdentifier::new("message_id", TType::I64, 5))?;
      o_prot.write_i64(fld_var)?;
      o_prot.write_field_end()?;
      ()
    } else {
      ()
    }
    if let Some(fld_var) = self.message_time_ns {
      o_prot.write_field_begin(&TFieldIdentifier::new("message_time_ns", TType::I64, 6))?;
      o_prot.write_i64(fld_var)?;
      o_prot.write_field_end()?;
      ()
    } else {
      ()
    }
    o_prot.write_field_stop()?;
    o_prot.write_struct_end()
  }
}

impl Default for Ingress {
  fn default() -> Self {
    Ingress{
      source: Some(0),
      receiver: Some(0),
      method_name: Some("".to_owned()),
      method_payload: Some(Vec::new()),
      message_id: Some(0),
      message_time_ns: Some(0),
    }
  }
}

