//! Types and gRPC stubs generated from `proto/bbs.proto`.
#![allow(dead_code)]

tonic::include_proto!("bbs.v2");

impl Message {
    /// Qualify a sender only when the receiving agent belongs to another human.
    pub fn sender_display_name(&self) -> String {
        match (&self.from_owner, &self.reader_owner) {
            (Some(owner), Some(reader))
                if self.from_kind() == Kind::Agent && !owner.eq_ignore_ascii_case(reader) =>
            {
                format!("{}@{owner}", self.from_name)
            }
            _ => self.from_name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    #[test]
    fn message_round_trips() {
        let msg = Message {
            id: 7,
            created_at: "2026-10-12T23:14:33Z".to_string(),
            from_kind: Kind::Agent as i32,
            from_name: "Ade".to_string(),
            from_owner: Some("will".to_string()),
            reader_owner: Some("ada".to_string()),
            to_group: Some("bbs-core".to_string()),
            to_name: None,
            read_at: None,
            body: "hello".to_string(),
        };
        let decoded = Message::decode(msg.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, msg);
    }
}
