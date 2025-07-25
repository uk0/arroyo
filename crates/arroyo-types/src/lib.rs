use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow_array::RecordBatch;
use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// worker configuration
pub const JOB_ID_ENV: &str = "JOB_ID";
pub const RUN_ID_ENV: &str = "RUN_ID";

// telemetry configuration
pub const TELEMETRY_KEY: &str = "phc_ghJo7Aa9QOo4inoWFYZP7o2aKszllEUyH77QeFgznUe";

// These seeds were randomly generated; changing them will break existing state
pub const HASH_SEEDS: [u64; 4] = [
    5093852630788334730,
    1843948808084437226,
    8049205638242432149,
    17942305062735447798,
];

#[derive(Debug, Hash, Eq, PartialEq, Copy, Clone)]
pub struct WorkerId(pub u64);

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub struct MachineId(pub Arc<String>);

impl Display for MachineId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub fn to_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

pub fn to_micros(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}

pub fn from_millis(ts: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ts)
}

pub fn from_micros(ts: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_micros(ts)
}

pub fn to_nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

pub fn from_nanos(ts: u128) -> SystemTime {
    UNIX_EPOCH
        + Duration::from_secs((ts / 1_000_000_000) as u64)
        + Duration::from_nanos((ts % 1_000_000_000) as u64)
}

pub fn print_time(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

pub fn single_item_hash_map<I: Into<K>, K: Hash + Eq, V>(key: I, value: V) -> HashMap<K, V> {
    let mut map = HashMap::new();
    map.insert(key.into(), value);
    map
}

// used for avro serialization -- returns the number of days since the UNIX EPOCH
pub fn days_since_epoch(time: SystemTime) -> i32 {
    time.duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .div_euclid(86400) as i32
}

pub fn string_to_map(s: &str, pair_delimeter: char) -> Option<HashMap<String, String>> {
    if s.trim().is_empty() {
        return Some(HashMap::new());
    }

    s.split(',')
        .map(|s| {
            let mut kv = s.trim().split(pair_delimeter);
            Some((kv.next()?.trim().to_string(), kv.next()?.trim().to_string()))
        })
        .collect()
}

pub trait Key:
    Debug + Clone + Encode + Decode<()> + Hash + PartialEq + Eq + Send + 'static
{
}
impl<T: Debug + Clone + Encode + Decode<()> + Hash + PartialEq + Eq + Send + 'static> Key for T {}

pub trait Data: Debug + Clone + Encode + Decode<()> + Send + 'static {}
impl<T: Debug + Clone + Encode + Decode<()> + Send + 'static> Data for T {}

#[derive(Debug, Copy, Clone, Encode, Decode, PartialEq, Eq)]
pub enum Watermark {
    EventTime(SystemTime),
    Idle,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrowMessage {
    Data(RecordBatch),
    Signal(SignalMessage),
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum SignalMessage {
    Barrier(CheckpointBarrier),
    Watermark(Watermark),
    Stop,
    EndOfData,
}

impl ArrowMessage {
    pub fn is_end(&self) -> bool {
        matches!(
            self,
            ArrowMessage::Signal(SignalMessage::Stop)
                | ArrowMessage::Signal(SignalMessage::EndOfData)
        )
    }
}

#[derive(Debug, Clone)]
pub struct UserError {
    pub name: String,
    pub details: String,
}

impl UserError {
    pub fn new(name: impl Into<String>, details: impl Into<String>) -> UserError {
        UserError {
            name: name.into(),
            details: details.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    BadData { details: String },
    Other { name: String, details: String },
}

impl SourceError {
    pub fn bad_data(details: impl Into<String>) -> SourceError {
        SourceError::BadData {
            details: details.into(),
        }
    }
    pub fn other(name: impl Into<String>, details: impl Into<String>) -> SourceError {
        SourceError::Other {
            name: name.into(),
            details: details.into(),
        }
    }

    pub fn details(&self) -> &String {
        match self {
            SourceError::BadData { details } | SourceError::Other { details, .. } => details,
        }
    }
}

#[derive(Debug, Clone, Encode, Decode, PartialEq, Serialize, Deserialize)]
pub enum UpdatingData<T: Data> {
    Retract(T),
    Update { old: T, new: T },
    Append(T),
}

impl<T: Data> UpdatingData<T> {
    pub fn lower(&self) -> T {
        match self {
            UpdatingData::Retract(_) => {
                panic!("cannot lower retractions")
            }
            UpdatingData::Update { new, .. } => new.clone(),
            UpdatingData::Append(t) => t.clone(),
        }
    }

    pub fn unwrap_append(&self) -> &T {
        match self {
            UpdatingData::Append(t) => t,
            _ => panic!("UpdatingData is not an append"),
        }
    }
}

#[derive(Clone, Encode, Decode, Debug, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "DebeziumShadow<T>")]
pub struct Debezium<T: Data> {
    pub before: Option<T>,
    pub after: Option<T>,
    pub op: DebeziumOp,
}

// Use a shadow type to perform post-deserialization validation that the expected fields
// are set; see https://github.com/serde-rs/serde/issues/642#issuecomment-683276351
#[derive(Clone, Encode, Decode, Debug, Serialize, Deserialize, PartialEq)]
struct DebeziumShadow<T: Data> {
    before: Option<T>,
    after: Option<T>,
    op: DebeziumOp,
}

impl<T: Data> TryFrom<DebeziumShadow<T>> for Debezium<T> {
    type Error = &'static str;

    fn try_from(value: DebeziumShadow<T>) -> Result<Self, Self::Error> {
        match (value.op, &value.before, &value.after) {
            (DebeziumOp::Create, _, None) => {
                Err("`after` must be set for Debezium create messages")
            }
            (DebeziumOp::Update, None, _) => {
                Err("`before` must be set for Debezium update messages; for Postgres you may need to set REPLICA IDENTIFY FULL on your database")
            }
            (DebeziumOp::Update, _, None) => {
                Err("`after` must be set for Debezium update messages")
            }
            (DebeziumOp::Delete, None, _) => {
                Err("`before` must be set for Debezium delete messages")
            }
            _ => Ok(Debezium {
                before: value.before,
                after: value.after,
                op: value.op,
            }),
        }
    }
}

//Debezium ops with single character serialization
#[derive(Copy, Clone, Encode, Decode, Debug, PartialEq)]
pub enum DebeziumOp {
    Create,
    Update,
    Delete,
}

#[allow(clippy::to_string_trait_impl)]
impl ToString for DebeziumOp {
    fn to_string(&self) -> String {
        match self {
            DebeziumOp::Create => "c",
            DebeziumOp::Update => "u",
            DebeziumOp::Delete => "d",
        }
        .to_string()
    }
}

impl<'de> Deserialize<'de> for DebeziumOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "c" => Ok(DebeziumOp::Create),
            "u" => Ok(DebeziumOp::Update),
            "d" => Ok(DebeziumOp::Delete),
            "r" => Ok(DebeziumOp::Create),
            _ => Err(serde::de::Error::custom(format!("Invalid DebeziumOp {s}"))),
        }
    }
}

impl Serialize for DebeziumOp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            DebeziumOp::Create => serializer.serialize_str("c"),
            DebeziumOp::Update => serializer.serialize_str("u"),
            DebeziumOp::Delete => serializer.serialize_str("d"),
        }
    }
}

#[derive(Copy, Clone, Encode, Decode, Debug, Serialize, Deserialize, PartialEq)]
pub enum JoinType {
    /// Inner Join
    Inner,
    /// Left Join
    Left,
    /// Right Join
    Right,
    /// Full Join
    Full,
}

pub trait RecordBatchBuilder: Default + Debug + Sync + Send + 'static {
    type Data: Data;
    fn nullable() -> Self;
    fn add_data(&mut self, data: Option<Self::Data>);
    fn flush(&mut self) -> RecordBatch;
    fn as_struct_array(&mut self) -> arrow::array::StructArray;
    fn schema(&self) -> SchemaRef;
}

#[derive(Eq, PartialEq, Hash, Debug, Clone, Encode, Decode)]
pub struct TaskInfo {
    pub job_id: String,
    pub node_id: u32,
    pub operator_name: String,
    pub operator_id: String,
    pub task_index: u32,
    pub parallelism: u32,
    pub key_range: RangeInclusive<u64>,
}

impl Display for TaskInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Task_{}-{}/{}",
            self.operator_id, self.task_index, self.parallelism
        )
    }
}

#[derive(Eq, PartialEq, Hash, Debug, Clone, Encode, Decode)]
pub struct ChainInfo {
    pub job_id: String,
    pub node_id: u32,
    pub description: String,
    pub task_index: u32,
}

impl Display for ChainInfo {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "TaskChain{}-{} ({})",
            self.node_id, self.task_index, self.description
        )
    }
}

impl ChainInfo {
    pub fn metric_label_map(&self) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.insert("node_id".to_string(), self.node_id.to_string());
        labels.insert("subtask_idx".to_string(), self.task_index.to_string());
        labels.insert("node_description".to_string(), self.description.to_string());
        labels
    }
}

impl TaskInfo {
    pub fn for_test(job_id: &str, operator_id: &str) -> Self {
        Self {
            job_id: job_id.to_string(),
            node_id: 1,
            operator_name: "op".to_string(),
            operator_id: operator_id.to_string(),
            task_index: 0,
            parallelism: 1,
            key_range: 0..=u64::MAX,
        }
    }
}

pub fn get_test_task_info() -> TaskInfo {
    TaskInfo {
        job_id: "instance-1".to_string(),
        node_id: 1,
        operator_name: "test-operator".to_string(),
        operator_id: "test-operator-1".to_string(),
        task_index: 0,
        parallelism: 1,
        key_range: 0..=u64::MAX,
    }
}

#[derive(Copy, Clone, Debug, Encode, Decode, Hash, PartialEq, Eq, serde::Serialize)]
pub struct GlobalKey {}

#[derive(Encode, Decode, Debug, Copy, Clone, Eq, PartialEq, serde::Serialize)]
pub struct ImpulseEvent {
    pub counter: u64,
    pub subtask_index: u64,
}

#[derive(Encode, Decode, Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RawJson {
    pub value: String,
}

pub fn raw_schema() -> Schema {
    Schema::new(vec![ArroyoExtensionType::add_metadata(
        Some(ArroyoExtensionType::JSON),
        Field::new("value", DataType::Utf8, false),
    )])
}

pub const MESSAGES_RECV: &str = "arroyo_worker_messages_recv";
pub const MESSAGES_SENT: &str = "arroyo_worker_messages_sent";
pub const BYTES_RECV: &str = "arroyo_worker_bytes_recv";
pub const BYTES_SENT: &str = "arroyo_worker_bytes_sent";
pub const BATCHES_RECV: &str = "arroyo_worker_batches_recv";
pub const BATCHES_SENT: &str = "arroyo_worker_batches_sent";
pub const TX_QUEUE_SIZE: &str = "arroyo_worker_tx_queue_size";
pub const TX_QUEUE_REM: &str = "arroyo_worker_tx_queue_rem";
pub const DESERIALIZATION_ERRORS: &str = "arroyo_worker_deserialization_errors";

#[derive(Debug, Copy, Clone, Encode, Decode, PartialEq, Eq)]
pub struct CheckpointBarrier {
    pub epoch: u32,
    pub min_epoch: u32,
    pub timestamp: SystemTime,
    pub then_stop: bool,
}

pub struct DisplayAsSql<'a>(pub &'a DataType);

impl Display for DisplayAsSql<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            DataType::Boolean => write!(f, "BOOLEAN"),
            DataType::Int8 | DataType::Int16 | DataType::Int32 => write!(f, "INT"),
            DataType::Int64 => write!(f, "BIGINT"),
            DataType::UInt8 | DataType::UInt16 | DataType::UInt32 => write!(f, "INT UNSIGNED"),
            DataType::UInt64 => write!(f, "BIGINT UNSIGNED"),
            DataType::Float16 | DataType::Float32 => write!(f, "FLOAT"),
            DataType::Float64 => write!(f, "DOUBLE"),
            DataType::Timestamp(_, _) => write!(f, "TIMESTAMP"),
            DataType::Date32 => write!(f, "DATE"),
            DataType::Date64 => write!(f, "DATETIME"),
            DataType::Time32(_) => write!(f, "TIME"),
            DataType::Time64(_) => write!(f, "TIME"),
            DataType::Duration(_) => write!(f, "INTERVAL"),
            DataType::Interval(_) => write!(f, "INTERVAL"),
            DataType::Binary | DataType::FixedSizeBinary(_) | DataType::LargeBinary => {
                write!(f, "BYTEA")
            }
            DataType::Utf8 | DataType::LargeUtf8 => write!(f, "TEXT"),
            DataType::List(inner) => {
                write!(f, "{}[]", DisplayAsSql(inner.data_type()))
            }
            dt => write!(f, "{dt}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Hash, Serialize)]
pub enum DatePart {
    Year,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
    DayOfWeek,
    DayOfYear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Serialize)]
pub enum DateTruncPrecision {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
}

impl TryFrom<&str> for DatePart {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let value_lower = value.to_lowercase();
        match value_lower.as_str() {
            "year" => Ok(DatePart::Year),
            "month" => Ok(DatePart::Month),
            "week" => Ok(DatePart::Week),
            "day" => Ok(DatePart::Day),
            "hour" => Ok(DatePart::Hour),
            "minute" => Ok(DatePart::Minute),
            "second" => Ok(DatePart::Second),
            "millisecond" => Ok(DatePart::Millisecond),
            "microsecond" => Ok(DatePart::Microsecond),
            "nanosecond" => Ok(DatePart::Nanosecond),
            "dow" => Ok(DatePart::DayOfWeek),
            "doy" => Ok(DatePart::DayOfYear),
            _ => Err(format!("'{value}' is not a valid DatePart")),
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ArroyoExtensionType {
    JSON,
}

impl ArroyoExtensionType {
    pub fn from_map(map: &HashMap<String, String>) -> Option<Self> {
        match map.get("ARROW:extension:name")?.as_str() {
            "arroyo.json" => Some(Self::JSON),
            _ => None,
        }
    }

    pub fn add_metadata(v: Option<Self>, field: Field) -> Field {
        if let Some(v) = v {
            let mut m = HashMap::new();
            match v {
                ArroyoExtensionType::JSON => {
                    m.insert(
                        "ARROW:extension:name".to_string(),
                        "arroyo.json".to_string(),
                    );
                }
            }
            field.with_metadata(m)
        } else {
            field
        }
    }
}

impl TryFrom<&str> for DateTruncPrecision {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let value_lower = value.to_lowercase();
        match value_lower.as_str() {
            "year" => Ok(DateTruncPrecision::Year),
            "quarter" => Ok(DateTruncPrecision::Quarter),
            "month" => Ok(DateTruncPrecision::Month),
            "week" => Ok(DateTruncPrecision::Week),
            "day" => Ok(DateTruncPrecision::Day),
            "hour" => Ok(DateTruncPrecision::Hour),
            "minute" => Ok(DateTruncPrecision::Minute),
            "second" => Ok(DateTruncPrecision::Second),

            _ => Err(format!("'{value}' is not a valid DateTruncPrecision")),
        }
    }
}

pub fn server_for_hash(x: u64, n: usize) -> usize {
    if n == 1 {
        0
    } else {
        let range_size = (u64::MAX / (n as u64)) + 1;
        (x / range_size) as usize
    }
}

pub fn range_for_server(i: usize, n: usize) -> RangeInclusive<u64> {
    if n == 1 {
        return 0..=u64::MAX;
    }
    let range_size = (u64::MAX / (n as u64)) + 1;
    let start = range_size * (i as u64);
    let end = if i + 1 == n {
        u64::MAX
    } else {
        start + range_size - 1
    };
    start..=end
}

pub const LOOKUP_KEY_INDEX_FIELD: &str = "__lookup_key_index";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_for_server() {
        let n = 6;

        for i in 0..(n - 1) {
            let range1 = range_for_server(i, n);
            let range2 = range_for_server(i + 1, n);

            assert_eq!(*range1.end() + 1, *range2.start(), "Ranges not adjacent");
            assert_eq!(
                i,
                server_for_hash(*range1.start(), n),
                "start not assigned to range."
            );
            assert_eq!(
                i,
                server_for_hash(*range1.end(), n),
                "end not assigned to range."
            );
        }

        let last_range = range_for_server(n - 1, n);
        assert_eq!(
            *last_range.end(),
            u64::MAX,
            "Last range does not contain u64::MAX"
        );
        assert_eq!(
            n - 1,
            server_for_hash(u64::MAX, n),
            "u64::MAX not in last range"
        );
    }

    #[test]
    fn test_server_for_hash() {
        let n = 2;
        let x = u64::MAX;

        let server_index = server_for_hash(x, n);
        let server_range = range_for_server(server_index, n);

        assert!(
            server_range.contains(&x),
            "u64::MAX is not in the correct range"
        );
    }
}
