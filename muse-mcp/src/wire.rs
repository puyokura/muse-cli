//! Protobuf wire codec for the Muse Hatch transport.
//!
//! The gateway speaks a small protobuf schema over the Noise channel:
//!
//! ```text
//! ServiceRequest   { uint32 service = 1; bytes payload = 2; }
//! ServiceResponse  { bytes payload = 1; }
//! ServiceFrame     { int64 stream_id = 1; ApplicationRequest request = 2;
//!                    ApplicationResponse response = 3; BodyChunk chunk = 4;
//!                    StreamReset reset = 5; }
//! ApplicationRequest  { string verb = 1; string path = 2; repeated Header headers = 3;
//!                       bytes body = 4; bool end_body = 5; }
//! ApplicationResponse { uint32 status = 1; repeated Header headers = 2; bytes body = 3;
//!                       bool end_body = 4; }
//! BodyChunk        { bytes data = 1; bool end_body = 2; }
//! NoiseChunk       { int64 chunk_id = 1; uint32 chunk_index = 2;
//!                    uint32 total_chunks = 3; bytes payload = 4; }
//! ```
//!
//! Field numbers follow the framing the shipped clients emit; the decoder is
//! tolerant of unknown fields so a server-side schema addition does not break
//! the transport.

pub const MAX_NOISE_CHUNK_PAYLOAD: usize = 65_489;
pub const MAX_NOISE_CHUNKS: usize = 256;
pub const MAX_ASSEMBLY_BYTES: usize = 16 * 1024 * 1024;

pub const SERVICE_DAEMON: u64 = 0;
#[allow(dead_code)]
pub const SERVICE_SENTINEL: u64 = 1;
#[allow(dead_code)]
pub const SERVICE_VAULT: u64 = 2;
#[allow(dead_code)]
pub const SERVICE_AUTHD: u64 = 3;

pub fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

pub fn tag(field: u32, wire_type: u8) -> Vec<u8> {
    varint(((field as u64) << 3) | wire_type as u64)
}

pub fn f_varint(field: u32, value: u64) -> Vec<u8> {
    let mut out = tag(field, 0);
    out.extend_from_slice(&varint(value));
    out
}

pub fn f_bytes(field: u32, value: &[u8]) -> Vec<u8> {
    let mut out = tag(field, 2);
    out.extend_from_slice(&varint(value.len() as u64));
    out.extend_from_slice(value);
    out
}

pub fn f_string(field: u32, value: &str) -> Vec<u8> {
    f_bytes(field, value.as_bytes())
}

pub fn f_bool(field: u32, value: bool) -> Vec<u8> {
    f_varint(field, if value { 1 } else { 0 })
}

fn read_varint(data: &[u8], mut pos: usize) -> Result<(u64, usize), String> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        if pos >= data.len() {
            return Err("protobuf: truncated varint".to_string());
        }
        let byte = data[pos];
        pos += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, pos));
        }
        shift += 7;
        if shift > 63 {
            return Err("protobuf: varint too long".to_string());
        }
    }
}

pub enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    #[allow(dead_code)]
    Fixed32(&'a [u8]),
    #[allow(dead_code)]
    Fixed64(&'a [u8]),
}

/// Iterate over `(field_number, value)` pairs in a protobuf message.
pub fn fields(data: &[u8]) -> Result<Vec<(u32, Field<'_>)>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let (key, next) = read_varint(data, pos)?;
        pos = next;
        let field = (key >> 3) as u32;
        let wire_type = (key & 0x07) as u8;
        match wire_type {
            0 => {
                let (value, next) = read_varint(data, pos)?;
                pos = next;
                out.push((field, Field::Varint(value)));
            }
            1 => {
                if pos + 8 > data.len() {
                    return Err("protobuf: truncated fixed64".to_string());
                }
                out.push((field, Field::Fixed64(&data[pos..pos + 8])));
                pos += 8;
            }
            2 => {
                let (len, next) = read_varint(data, pos)?;
                pos = next;
                let end = pos + len as usize;
                if end > data.len() {
                    return Err("protobuf: truncated length-delimited field".to_string());
                }
                out.push((field, Field::Bytes(&data[pos..end])));
                pos = end;
            }
            5 => {
                if pos + 4 > data.len() {
                    return Err("protobuf: truncated fixed32".to_string());
                }
                out.push((field, Field::Fixed32(&data[pos..pos + 4])));
                pos += 4;
            }
            other => return Err(format!("protobuf: unsupported wire type {other}")),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Header {
    pub key: String,
    pub value: String,
}

impl Header {
    pub fn new(key: &str, value: &str) -> Self {
        Self {
            key: key.to_string(),
            value: value.to_string(),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = f_string(1, &self.key);
        out.extend_from_slice(&f_string(2, &self.value));
        out
    }
}

pub struct ApplicationRequest {
    pub verb: String,
    pub path: String,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
    pub end_body: bool,
}

impl ApplicationRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = f_string(1, &self.verb);
        out.extend_from_slice(&f_string(2, &self.path));
        for header in &self.headers {
            out.extend_from_slice(&f_bytes(3, &header.encode()));
        }
        if !self.body.is_empty() {
            out.extend_from_slice(&f_bytes(4, &self.body));
        }
        out.extend_from_slice(&f_bool(5, self.end_body));
        out
    }
}

#[derive(Debug, Clone, Default)]
pub struct ApplicationResponse {
    pub status: u32,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
    pub end_body: bool,
}

impl ApplicationResponse {
    pub fn decode(data: &[u8]) -> Result<Self, String> {
        let mut response = ApplicationResponse::default();
        for (field, value) in fields(data)? {
            match (field, value) {
                (1, Field::Varint(status)) => response.status = status as u32,
                (2, Field::Bytes(inner)) => {
                    let mut key = String::new();
                    let mut val = String::new();
                    for (f2, v2) in fields(inner)? {
                        match (f2, v2) {
                            (1, Field::Bytes(raw)) => key = String::from_utf8_lossy(raw).into_owned(),
                            (2, Field::Bytes(raw)) => val = String::from_utf8_lossy(raw).into_owned(),
                            _ => {}
                        }
                    }
                    response.headers.push(Header { key, value: val });
                }
                (3, Field::Bytes(body)) => response.body.extend_from_slice(body),
                (4, Field::Varint(flag)) => response.end_body = flag != 0,
                _ => {}
            }
        }
        Ok(response)
    }
}

pub enum FramePayload {
    Response(ApplicationResponse),
    BodyChunk { data: Vec<u8>, end_body: bool },
    Reset { code: u64, reason: String },
}

pub struct ServiceFrame {
    pub stream_id: i64,
    pub payload: FramePayload,
}

pub fn encode_service_frame_request(stream_id: i64, request: &ApplicationRequest) -> Vec<u8> {
    let mut out = f_varint(1, stream_id as u64);
    out.extend_from_slice(&f_bytes(2, &request.encode()));
    out
}

pub fn decode_service_frame(data: &[u8]) -> Result<ServiceFrame, String> {
    let mut stream_id = 0i64;
    let mut payload: Option<FramePayload> = None;
    for (field, value) in fields(data)? {
        match (field, value) {
            (1, Field::Varint(raw)) => stream_id = raw as i64,
            (3, Field::Bytes(raw)) => {
                payload = Some(FramePayload::Response(ApplicationResponse::decode(raw)?))
            }
            (4, Field::Bytes(raw)) => {
                let mut chunk_data = Vec::new();
                let mut end_body = false;
                for (f2, v2) in fields(raw)? {
                    match (f2, v2) {
                        (1, Field::Bytes(bytes)) => chunk_data.extend_from_slice(bytes),
                        (2, Field::Varint(flag)) => end_body = flag != 0,
                        _ => {}
                    }
                }
                payload = Some(FramePayload::BodyChunk {
                    data: chunk_data,
                    end_body,
                });
            }
            (5, Field::Bytes(raw)) => {
                let mut code = 0u64;
                let mut reason = String::new();
                for (f2, v2) in fields(raw)? {
                    match (f2, v2) {
                        (1, Field::Varint(value)) => code = value,
                        (2, Field::Bytes(bytes)) => {
                            reason = String::from_utf8_lossy(bytes).into_owned()
                        }
                        _ => {}
                    }
                }
                payload = Some(FramePayload::Reset { code, reason });
            }
            _ => {}
        }
    }
    match payload {
        Some(payload) => Ok(ServiceFrame { stream_id, payload }),
        None => Err("protobuf: service frame has no payload".to_string()),
    }
}

pub fn encode_service_request(service: u64, service_frame: &[u8]) -> Vec<u8> {
    let mut out = f_varint(1, service);
    out.extend_from_slice(&f_bytes(2, service_frame));
    out
}

pub fn decode_service_response(data: &[u8]) -> Result<Vec<u8>, String> {
    for (field, value) in fields(data)? {
        if let (1, Field::Bytes(raw)) = (field, value) {
            return Ok(raw.to_vec());
        }
    }
    Err("protobuf: ServiceResponse has no payload".to_string())
}

pub struct NoiseChunk {
    pub chunk_id: i64,
    pub chunk_index: u32,
    pub total_chunks: u32,
    pub payload: Vec<u8>,
}

impl NoiseChunk {
    fn encode(&self) -> Vec<u8> {
        let mut out = f_varint(1, self.chunk_id as u64);
        out.extend_from_slice(&f_varint(2, self.chunk_index as u64));
        out.extend_from_slice(&f_varint(3, self.total_chunks as u64));
        out.extend_from_slice(&f_bytes(4, &self.payload));
        out
    }

    fn decode(data: &[u8]) -> Result<Self, String> {
        let mut chunk = NoiseChunk {
            chunk_id: 0,
            chunk_index: 0,
            total_chunks: 1,
            payload: Vec::new(),
        };
        for (field, value) in fields(data)? {
            match (field, value) {
                (1, Field::Varint(raw)) => chunk.chunk_id = raw as i64,
                (2, Field::Varint(raw)) => chunk.chunk_index = raw as u32,
                (3, Field::Varint(raw)) => chunk.total_chunks = raw as u32,
                (4, Field::Bytes(raw)) => chunk.payload = raw.to_vec(),
                _ => {}
            }
        }
        Ok(chunk)
    }
}

pub fn split_noise_payload(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let total = if data.is_empty() {
        1
    } else {
        (data.len() + MAX_NOISE_CHUNK_PAYLOAD - 1) / MAX_NOISE_CHUNK_PAYLOAD
    };
    if total > MAX_NOISE_CHUNKS {
        return Err(format!(
            "payload too large for Noise framing: {total} chunks (max {MAX_NOISE_CHUNKS})"
        ));
    }
    let chunk_id: i64 = rand::random();
    if data.is_empty() {
        return Ok(vec![NoiseChunk {
            chunk_id,
            chunk_index: 0,
            total_chunks: 1,
            payload: Vec::new(),
        }
        .encode()]);
    }
    let mut out = Vec::with_capacity(total);
    for index in 0..total {
        let start = index * MAX_NOISE_CHUNK_PAYLOAD;
        let end = usize::min(start + MAX_NOISE_CHUNK_PAYLOAD, data.len());
        out.push(
            NoiseChunk {
                chunk_id,
                chunk_index: index as u32,
                total_chunks: total as u32,
                payload: data[start..end].to_vec(),
            }
            .encode(),
        );
    }
    Ok(out)
}

pub struct Reassembler {
    pending: std::collections::HashMap<i64, (u32, Vec<Option<Vec<u8>>>, usize)>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            pending: std::collections::HashMap::new(),
        }
    }

    pub fn feed(&mut self, raw: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let chunk = NoiseChunk::decode(raw)?;
        if chunk.total_chunks < 1 || chunk.total_chunks as usize > MAX_NOISE_CHUNKS {
            return Err("noise chunk has invalid total_chunks".to_string());
        }
        if chunk.chunk_index >= chunk.total_chunks {
            return Err("noise chunk has invalid chunk_index".to_string());
        }
        if chunk.payload.len() > MAX_NOISE_CHUNK_PAYLOAD {
            return Err("noise chunk payload too large".to_string());
        }

        let entry = self
            .pending
            .entry(chunk.chunk_id)
            .or_insert_with(|| (chunk.total_chunks, vec![None; chunk.total_chunks as usize], 0));
        if entry.0 != chunk.total_chunks {
            self.pending.remove(&chunk.chunk_id);
            return Err("noise chunk has inconsistent total_chunks".to_string());
        }
        let slot = chunk.chunk_index as usize;
        if entry.1[slot].is_some() {
            self.pending.remove(&chunk.chunk_id);
            return Err("duplicate noise chunk".to_string());
        }
        entry.2 += chunk.payload.len();
        if entry.2 > MAX_ASSEMBLY_BYTES {
            self.pending.remove(&chunk.chunk_id);
            return Err("noise assembly exceeded 16 MiB".to_string());
        }
        let received = {
            entry.1[slot] = Some(chunk.payload);
            entry.1.iter().filter(|part| part.is_some()).count()
        };
        if received != chunk.total_chunks as usize {
            return Ok(None);
        }
        let (_, parts, _) = self.pending.remove(&chunk.chunk_id).expect("entry");
        let mut assembled = Vec::new();
        for part in parts {
            assembled.extend_from_slice(&part.unwrap_or_default());
        }
        Ok(Some(assembled))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip() {
        for value in [0u64, 1, 127, 128, 255, 256, 65535, u32::MAX as u64] {
            let encoded = varint(value);
            let (decoded, pos) = read_varint(&encoded, 0).expect("decode");
            assert_eq!(decoded, value);
            assert_eq!(pos, encoded.len());
        }
    }

    #[test]
    fn application_request_round_trip() {
        let request = ApplicationRequest {
            verb: "POST".to_string(),
            path: "/chat/stream".to_string(),
            headers: vec![Header::new("Content-Type", "application/json")],
            body: b"{\"items\":[]}".to_vec(),
            end_body: true,
        };
        let encoded = request.encode();
        let fields = fields(&encoded).expect("parse");

        let mut verb = String::new();
        let mut path = String::new();
        let mut body = Vec::new();
        let mut header_count = 0usize;
        for (field, value) in fields {
            match (field, value) {
                (1, Field::Bytes(bytes)) => verb = String::from_utf8_lossy(bytes).into_owned(),
                (2, Field::Bytes(bytes)) => path = String::from_utf8_lossy(bytes).into_owned(),
                (3, Field::Bytes(_)) => header_count += 1,
                (4, Field::Bytes(bytes)) => body = bytes.to_vec(),
                _ => {}
            }
        }
        assert_eq!(verb, "POST");
        assert_eq!(path, "/chat/stream");
        assert_eq!(header_count, 1);
        assert_eq!(body, b"{\"items\":[]}");
    }

    #[test]
    fn service_frame_decode_reads_status_and_body() {
        let response = ApplicationResponse {
            status: 200,
            headers: vec![Header::new("content-type", "application/json")],
            body: b"{\"ok\":true}".to_vec(),
            end_body: true,
        };
        let mut encoded = f_varint(1, 7);
        encoded.extend_from_slice(&f_bytes(3, &encode_response(&response)));

        let frame = decode_service_frame(&encoded).expect("decode");
        assert_eq!(frame.stream_id, 7);
        match frame.payload {
            FramePayload::Response(reply) => {
                assert_eq!(reply.status, 200);
                assert_eq!(reply.body, b"{\"ok\":true}");
                assert!(reply.end_body);
                assert_eq!(reply.headers.len(), 1);
            }
            _ => panic!("expected a response frame"),
        }
    }

    #[test]
    fn body_chunk_and_reset_frames_decode() {
        let chunk = f_bytes(1, b"chunk").into_iter().chain(f_bool(2, true)).collect::<Vec<u8>>();
        let mut frame = f_varint(1, 3);
        frame.extend_from_slice(&f_bytes(4, &chunk));
        match decode_service_frame(&frame).expect("chunk").payload {
            FramePayload::BodyChunk { data, end_body } => {
                assert_eq!(data, b"chunk");
                assert!(end_body);
            }
            _ => panic!("expected a body chunk"),
        }

        let reset = f_varint(1, 4294967296u64).into_iter().chain(f_string(2, "closed")).collect::<Vec<u8>>();
        let mut frame = f_varint(1, 5);
        frame.extend_from_slice(&f_bytes(5, &reset));
        match decode_service_frame(&frame).expect("reset").payload {
            FramePayload::Reset { code, reason } => {
                assert_eq!(code, 4294967296);
                assert_eq!(reason, "closed");
            }
            _ => panic!("expected a reset"),
        }
    }

    #[test]
    fn service_response_unwraps_payload() {
        let inner = encode_response(&ApplicationResponse {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
            end_body: true,
        });
        let envelope = f_bytes(1, &inner);
        let decoded = decode_service_response(&envelope).expect("unwrap");
        assert_eq!(decoded, inner);
        assert!(decode_service_response(&f_varint(1, 9)).is_err());
    }

    #[test]
    fn noise_chunks_split_and_reassemble() {
        let payload: Vec<u8> = (0..(MAX_NOISE_CHUNK_PAYLOAD * 2 + 100))
            .map(|i| (i % 251) as u8)
            .collect();
        let chunks = split_noise_payload(&payload).expect("split");
        assert_eq!(chunks.len(), 3);

        let mut reassembler = Reassembler::new();
        let mut assembled: Option<Vec<u8>> = None;
        for chunk in chunks {
            if let Some(whole) = reassembler.feed(&chunk).expect("feed") {
                assembled = Some(whole);
            }
        }
        assert_eq!(assembled.expect("assembled"), payload);
    }

    #[test]
    fn empty_payload_still_produces_one_chunk() {
        let chunks = split_noise_payload(b"").expect("split");
        assert_eq!(chunks.len(), 1);
        let mut reassembler = Reassembler::new();
        let whole = reassembler.feed(&chunks[0]).expect("feed");
        assert_eq!(whole, Some(Vec::new()));
    }

    #[test]
    fn reassembler_rejects_inconsistent_total() {
        let chunk = NoiseChunk {
            chunk_id: 1,
            chunk_index: 0,
            total_chunks: 2,
            payload: b"a".to_vec(),
        }
        .encode();
        let mut reassembler = Reassembler::new();
        assert!(reassembler.feed(&chunk).expect("first").is_none());

        let inconsistent = NoiseChunk {
            chunk_id: 1,
            chunk_index: 1,
            total_chunks: 3,
            payload: b"b".to_vec(),
        }
        .encode();
        assert!(reassembler.feed(&inconsistent).is_err());
    }

    #[test]
    fn reassembler_rejects_duplicate_chunk() {
        let make = |index: u32| {
            NoiseChunk {
                chunk_id: 9,
                chunk_index: index,
                total_chunks: 2,
                payload: b"x".to_vec(),
            }
            .encode()
        };
        let mut reassembler = Reassembler::new();
        assert!(reassembler.feed(&make(0)).expect("first").is_none());
        assert!(reassembler.feed(&make(0)).is_err());
    }

    #[test]
    fn truncated_message_is_an_error_not_a_panic() {
        assert!(fields(&[0x0a]).is_err());
        assert!(fields(&[0x0a, 0x05, 0x41]).is_err());
    }

    /// Encode an `ApplicationResponse` for the frame tests. Mirrors the field
    /// numbers in `ApplicationResponse::decode`.
    fn encode_response(response: &ApplicationResponse) -> Vec<u8> {
        let mut out = f_varint(1, response.status as u64);
        for header in &response.headers {
            let mut inner = f_string(1, &header.key);
            inner.extend_from_slice(&f_string(2, &header.value));
            out.extend_from_slice(&f_bytes(2, &inner));
        }
        if !response.body.is_empty() {
            out.extend_from_slice(&f_bytes(3, &response.body));
        }
        out.extend_from_slice(&f_bool(4, response.end_body));
        out
    }
}
