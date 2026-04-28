//! Data adapters for canonical Kenoma market events.

use anyhow::{Context, Result};
#[cfg(feature = "dbn")]
use kenoma_book::BookBuilder;
use kenoma_book::{flow::FlowState, CommittedState};
use kenoma_types::{Bar, MarketEvent, MboEvent};
#[cfg(feature = "parquet")]
use kenoma_types::{EquityPoint, Fill, OrderRequest, Position};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("unsupported data source kind: {0}")]
    UnsupportedSource(String),
    #[error("DBN support was not enabled at compile time")]
    DbnFeatureDisabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataSourceKind {
    JsonlEvents,
    BarCsv,
    BarParquet,
    DbnMbo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSourceConfig {
    pub kind: DataSourceKind,
    pub path: PathBuf,
    #[serde(default)]
    pub instrument_id: Option<u32>,
    #[serde(default)]
    pub date: Option<String>,
}

pub trait DataAdapter {
    fn load_events(&self) -> Result<Vec<MarketEvent>>;
}

impl DataAdapter for DataSourceConfig {
    fn load_events(&self) -> Result<Vec<MarketEvent>> {
        match self.kind {
            DataSourceKind::JsonlEvents => read_jsonl_events(&self.path),
            DataSourceKind::BarCsv => read_bar_csv(&self.path),
            DataSourceKind::BarParquet => {
                #[cfg(feature = "parquet")]
                {
                    read_bar_parquet(&self.path)
                }
                #[cfg(not(feature = "parquet"))]
                {
                    Err(DataError::UnsupportedSource("bar_parquet".to_string()).into())
                }
            }
            DataSourceKind::DbnMbo => {
                #[cfg(feature = "dbn")]
                {
                    let instrument_id = self
                        .instrument_id
                        .context("dbn_mbo source requires instrument_id")?;
                    let date = self.date.as_deref().unwrap_or("");
                    let result = ingest_dbn_mbo(&self.path, instrument_id, date)?;
                    Ok(result
                        .mbo_events
                        .into_iter()
                        .map(MarketEvent::Mbo)
                        .collect())
                }
                #[cfg(not(feature = "dbn"))]
                {
                    Err(DataError::DbnFeatureDisabled.into())
                }
            }
        }
    }
}

pub fn load_sources(sources: &[DataSourceConfig]) -> Result<Vec<MarketEvent>> {
    let mut events = Vec::new();
    for source in sources {
        events.extend(source.load_events()?);
    }
    events.sort_by_key(|event| (event.timestamp_ns(), event.priority()));
    Ok(events)
}

pub fn read_jsonl_events(path: impl AsRef<Path>) -> Result<Vec<MarketEvent>> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (line_idx, line) in reader.lines().enumerate() {
        let line =
            line.with_context(|| format!("reading {} line {}", path.display(), line_idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<MarketEvent>(&line)
            .with_context(|| format!("parsing {} line {}", path.display(), line_idx + 1))?;
        events.push(event);
    }
    events.sort_by_key(|event| (event.timestamp_ns(), event.priority()));
    Ok(events)
}

pub fn write_jsonl_events(path: impl AsRef<Path>, events: &[MarketEvent]) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for event in events {
        serde_json::to_writer(&mut writer, event)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct BarCsvRow {
    instrument_id: u32,
    ts_open: u64,
    ts_close: u64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    #[serde(default)]
    volume: f64,
}

pub fn read_bar_csv(path: impl AsRef<Path>) -> Result<Vec<MarketEvent>> {
    let path = path.as_ref();
    let mut rdr = csv::Reader::from_path(path)
        .with_context(|| format!("opening bar csv {}", path.display()))?;
    let mut events = Vec::new();
    for row in rdr.deserialize::<BarCsvRow>() {
        let row = row?;
        events.push(MarketEvent::Bar(Bar {
            instrument_id: row.instrument_id,
            ts_open: row.ts_open,
            ts_close: row.ts_close,
            open: row.open,
            high: row.high,
            low: row.low,
            close: row.close,
            volume: row.volume,
            vwap: None,
            feature_cutoff_ts: Some(row.ts_close),
        }));
    }
    events.sort_by_key(|event| (event.timestamp_ns(), event.priority()));
    Ok(events)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalSchema {
    pub bars: Vec<FieldSpec>,
    pub quotes: Vec<FieldSpec>,
    pub trades: Vec<FieldSpec>,
    pub mbo: Vec<FieldSpec>,
    pub orders: Vec<FieldSpec>,
    pub fills: Vec<FieldSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSpec {
    pub name: String,
    pub dtype: String,
    pub nullable: bool,
}

pub fn canonical_schema() -> CanonicalSchema {
    CanonicalSchema {
        bars: vec![
            field("instrument_id", "uint32", false),
            field("ts_open", "uint64", false),
            field("ts_close", "uint64", false),
            field("open", "float64", false),
            field("high", "float64", false),
            field("low", "float64", false),
            field("close", "float64", false),
            field("volume", "float64", false),
            field("vwap", "float64", true),
            field("feature_cutoff_ts", "uint64", true),
        ],
        quotes: vec![
            field("instrument_id", "uint32", false),
            field("ts", "uint64", false),
            field("bid_price", "float64", false),
            field("bid_size", "float64", false),
            field("ask_price", "float64", false),
            field("ask_size", "float64", false),
        ],
        trades: vec![
            field("instrument_id", "uint32", false),
            field("ts", "uint64", false),
            field("price", "float64", false),
            field("size", "float64", false),
            field("aggressor_side", "enum", false),
        ],
        mbo: vec![
            field("instrument_id", "uint32", false),
            field("ts", "uint64", false),
            field("order_id", "uint64", false),
            field("action", "enum", false),
            field("side", "enum", false),
            field("price_fixed", "int64", false),
            field("size", "uint32", false),
            field("flags", "uint8", false),
        ],
        orders: vec![
            field("id", "uint64", false),
            field("instrument_id", "uint32", false),
            field("created_ts", "uint64", false),
            field("side", "enum", false),
            field("qty", "float64", false),
            field("order_type", "struct", false),
            field("tag", "utf8", true),
        ],
        fills: vec![
            field("order_id", "uint64", false),
            field("instrument_id", "uint32", false),
            field("ts", "uint64", false),
            field("side", "enum", false),
            field("price", "float64", false),
            field("qty", "float64", false),
            field("fee", "float64", false),
            field("liquidity", "utf8", true),
        ],
    }
}

fn field(name: &'static str, dtype: &'static str, nullable: bool) -> FieldSpec {
    FieldSpec {
        name: name.to_string(),
        dtype: dtype.to_string(),
        nullable,
    }
}

#[derive(Debug)]
pub struct DbnMboIngestResult {
    pub first_ts: u64,
    pub last_ts: u64,
    pub total_records: u64,
    pub instrument_records: u64,
    pub mbo_events: Vec<MboEvent>,
    pub committed_states: Vec<CommittedState>,
    pub flow_states: Vec<FlowState>,
}

#[cfg(feature = "dbn")]
pub fn ingest_dbn_mbo(
    path: impl AsRef<Path>,
    instrument_id: u32,
    _date: &str,
) -> Result<DbnMboIngestResult> {
    use dbn::decode::{DbnDecoder, DecodeRecord};
    use dbn::MboMsg;

    let path = path.as_ref();
    let mut decoder =
        DbnDecoder::from_zstd_file(path).with_context(|| format!("opening {}", path.display()))?;
    let mut builder = BookBuilder::new(instrument_id);
    let mut first_ts = None;
    let mut last_ts = 0;
    let mut total_records = 0;
    let mut instrument_records = 0;
    let mut mbo_events = Vec::new();
    let mut committed_states = Vec::new();
    let mut flow_states = Vec::new();

    while let Some(msg) = decoder.decode_record::<MboMsg>()? {
        total_records += 1;
        let ts = msg.hd.ts_event;
        let id = msg.hd.instrument_id;
        let action = msg.action as u8 as char;
        let side = msg.side as u8 as char;
        let flags = msg.flags.raw();
        let event = MboEvent::from_dbn_parts(
            ts,
            msg.order_id,
            id,
            action,
            side,
            msg.price,
            msg.size,
            flags,
        );
        builder.process_mbo(&event);
        if id == instrument_id {
            instrument_records += 1;
            if first_ts.is_none() {
                first_ts = Some(ts);
            }
            last_ts = ts;
            mbo_events.push(event);
            if flags & kenoma_book::F_LAST != 0 {
                committed_states.push(builder.current_committed_state(ts));
                let mut flow = builder.current_flow_state();
                flow.ts = ts;
                flow_states.push(flow);
            }
        }
    }

    Ok(DbnMboIngestResult {
        first_ts: first_ts.unwrap_or(0),
        last_ts,
        total_records,
        instrument_records,
        mbo_events,
        committed_states,
        flow_states,
    })
}

pub fn dbn_file_path(data_dir: &str, date: &str) -> String {
    format!("{data_dir}/glbx-mdp3-{date}.mbo.dbn.zst")
}

#[cfg(feature = "parquet")]
mod parquet_artifacts {
    use super::*;
    use arrow::array::{Array, ArrayRef, Float64Array, StringArray, UInt32Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    pub fn read_bar_parquet(path: impl AsRef<Path>) -> Result<Vec<MarketEvent>> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let reader = builder.build()?;
        let mut events = Vec::new();
        for batch in reader {
            let batch = batch?;
            let instrument_id = column::<UInt32Array>(&batch, "instrument_id")?;
            let ts_open = column::<UInt64Array>(&batch, "ts_open")?;
            let ts_close = column::<UInt64Array>(&batch, "ts_close")?;
            let open = column::<Float64Array>(&batch, "open")?;
            let high = column::<Float64Array>(&batch, "high")?;
            let low = column::<Float64Array>(&batch, "low")?;
            let close = column::<Float64Array>(&batch, "close")?;
            let volume = column::<Float64Array>(&batch, "volume")?;
            let vwap = optional_column::<Float64Array>(&batch, "vwap")?;
            let feature_cutoff_ts = optional_column::<UInt64Array>(&batch, "feature_cutoff_ts")?;
            for row in 0..batch.num_rows() {
                check_not_null(instrument_id, "instrument_id", row)?;
                check_not_null(ts_open, "ts_open", row)?;
                check_not_null(ts_close, "ts_close", row)?;
                check_not_null(open, "open", row)?;
                check_not_null(high, "high", row)?;
                check_not_null(low, "low", row)?;
                check_not_null(close, "close", row)?;
                check_not_null(volume, "volume", row)?;
                events.push(MarketEvent::Bar(Bar {
                    instrument_id: instrument_id.value(row),
                    ts_open: ts_open.value(row),
                    ts_close: ts_close.value(row),
                    open: open.value(row),
                    high: high.value(row),
                    low: low.value(row),
                    close: close.value(row),
                    volume: volume.value(row),
                    vwap: vwap.and_then(|array| (!array.is_null(row)).then(|| array.value(row))),
                    feature_cutoff_ts: feature_cutoff_ts
                        .and_then(|array| (!array.is_null(row)).then(|| array.value(row))),
                }));
            }
        }
        events.sort_by_key(|event| (event.timestamp_ns(), event.priority()));
        Ok(events)
    }

    pub fn write_bars_parquet(path: impl AsRef<Path>, bars: &[Bar]) -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("instrument_id", DataType::UInt32, false),
            Field::new("ts_open", DataType::UInt64, false),
            Field::new("ts_close", DataType::UInt64, false),
            Field::new("open", DataType::Float64, false),
            Field::new("high", DataType::Float64, false),
            Field::new("low", DataType::Float64, false),
            Field::new("close", DataType::Float64, false),
            Field::new("volume", DataType::Float64, false),
            Field::new("vwap", DataType::Float64, true),
            Field::new("feature_cutoff_ts", DataType::UInt64, true),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt32Array::from(
                bars.iter().map(|bar| bar.instrument_id).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                bars.iter().map(|bar| bar.ts_open).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                bars.iter().map(|bar| bar.ts_close).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                bars.iter().map(|bar| bar.open).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                bars.iter().map(|bar| bar.high).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                bars.iter().map(|bar| bar.low).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                bars.iter().map(|bar| bar.close).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                bars.iter().map(|bar| bar.volume).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                bars.iter()
                    .map(|bar| bar.vwap)
                    .collect::<Vec<Option<f64>>>(),
            )),
            Arc::new(UInt64Array::from(
                bars.iter()
                    .map(|bar| bar.feature_cutoff_ts)
                    .collect::<Vec<Option<u64>>>(),
            )),
        ];
        write_batch(path, schema, columns)
    }

    pub fn write_orders_parquet(path: impl AsRef<Path>, orders: &[OrderRequest]) -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("instrument_id", DataType::UInt32, false),
            Field::new("created_ts", DataType::UInt64, false),
            Field::new("side", DataType::Utf8, false),
            Field::new("qty", DataType::Float64, false),
            Field::new("order_type", DataType::Utf8, false),
            Field::new("tag", DataType::Utf8, true),
        ]));
        let order_types = orders
            .iter()
            .map(|order| serde_json::to_string(&order.order_type))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(
                orders.iter().map(|order| order.id).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                orders
                    .iter()
                    .map(|order| order.instrument_id)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                orders
                    .iter()
                    .map(|order| order.created_ts)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                orders
                    .iter()
                    .map(|order| format!("{:?}", order.side).to_lowercase())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                orders.iter().map(|order| order.qty).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(order_types)),
            Arc::new(StringArray::from(
                orders
                    .iter()
                    .map(|order| order.tag.as_deref())
                    .collect::<Vec<Option<&str>>>(),
            )),
        ];
        write_batch(path, schema, columns)
    }

    pub fn write_fills_parquet(path: impl AsRef<Path>, fills: &[Fill]) -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("order_id", DataType::UInt64, false),
            Field::new("instrument_id", DataType::UInt32, false),
            Field::new("ts", DataType::UInt64, false),
            Field::new("side", DataType::Utf8, false),
            Field::new("price", DataType::Float64, false),
            Field::new("qty", DataType::Float64, false),
            Field::new("fee", DataType::Float64, false),
            Field::new("liquidity", DataType::Utf8, true),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(
                fills.iter().map(|fill| fill.order_id).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                fills
                    .iter()
                    .map(|fill| fill.instrument_id)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                fills.iter().map(|fill| fill.ts).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                fills
                    .iter()
                    .map(|fill| format!("{:?}", fill.side).to_lowercase())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                fills.iter().map(|fill| fill.price).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                fills.iter().map(|fill| fill.qty).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                fills.iter().map(|fill| fill.fee).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                fills
                    .iter()
                    .map(|fill| fill.liquidity.as_deref())
                    .collect::<Vec<Option<&str>>>(),
            )),
        ];
        write_batch(path, schema, columns)
    }

    pub fn write_positions_parquet(path: impl AsRef<Path>, positions: &[Position]) -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("instrument_id", DataType::UInt32, false),
            Field::new("qty", DataType::Float64, false),
            Field::new("avg_price", DataType::Float64, false),
            Field::new("realized_pnl", DataType::Float64, false),
            Field::new("fees", DataType::Float64, false),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt32Array::from(
                positions
                    .iter()
                    .map(|position| position.instrument_id)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                positions
                    .iter()
                    .map(|position| position.qty)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                positions
                    .iter()
                    .map(|position| position.avg_price)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                positions
                    .iter()
                    .map(|position| position.realized_pnl)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                positions
                    .iter()
                    .map(|position| position.fees)
                    .collect::<Vec<_>>(),
            )),
        ];
        write_batch(path, schema, columns)
    }

    pub fn write_equity_curve_parquet(
        path: impl AsRef<Path>,
        equity_curve: &[EquityPoint],
    ) -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("ts", DataType::UInt64, false),
            Field::new("equity", DataType::Float64, false),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(
                equity_curve
                    .iter()
                    .map(|point| point.ts)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                equity_curve
                    .iter()
                    .map(|point| point.equity)
                    .collect::<Vec<_>>(),
            )),
        ];
        write_batch(path, schema, columns)
    }

    fn write_batch(
        path: impl AsRef<Path>,
        schema: Arc<Schema>,
        columns: Vec<ArrayRef>,
    ) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let batch = RecordBatch::try_new(schema.clone(), columns)?;
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut writer = ArrowWriter::try_new(file, schema, None)?;
        writer.write(&batch)?;
        writer.close()?;
        Ok(())
    }

    fn column<'a, T: Array + 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
        let idx = batch
            .schema()
            .index_of(name)
            .with_context(|| format!("missing parquet column {name}"))?;
        batch
            .column(idx)
            .as_any()
            .downcast_ref::<T>()
            .with_context(|| format!("parquet column {name} has unexpected type"))
    }

    fn optional_column<'a, T: Array + 'static>(
        batch: &'a RecordBatch,
        name: &str,
    ) -> Result<Option<&'a T>> {
        let Ok(idx) = batch.schema().index_of(name) else {
            return Ok(None);
        };
        let array = batch
            .column(idx)
            .as_any()
            .downcast_ref::<T>()
            .with_context(|| format!("parquet column {name} has unexpected type"))?;
        Ok(Some(array))
    }

    fn check_not_null(array: &dyn Array, name: &str, row: usize) -> Result<()> {
        if array.is_null(row) {
            anyhow::bail!("parquet column {name} has null at row {row}");
        }
        Ok(())
    }
}

#[cfg(feature = "parquet")]
pub use parquet_artifacts::{
    read_bar_parquet, write_bars_parquet, write_equity_curve_parquet, write_fills_parquet,
    write_orders_parquet, write_positions_parquet,
};

#[cfg(test)]
mod tests {
    use super::*;
    use kenoma_types::{Fill, MarketEvent, OrderRequest, OrderSide, Quote};

    #[test]
    fn schema_contains_required_fill_fields() {
        let schema = canonical_schema();
        assert!(schema.fills.iter().any(|field| field.name == "order_id"));
        assert!(schema.mbo.iter().any(|field| field.name == "price_fixed"));
    }

    #[test]
    fn jsonl_round_trip_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let events = vec![MarketEvent::Quote(Quote {
            instrument_id: 1,
            ts: 10,
            bid_price: 99.0,
            bid_size: 1.0,
            ask_price: 100.0,
            ask_size: 1.0,
        })];
        write_jsonl_events(&path, &events).unwrap();
        let loaded = read_jsonl_events(&path).unwrap();
        assert_eq!(loaded, events);
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn bar_parquet_round_trip_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars.parquet");
        let bars = vec![Bar {
            instrument_id: 1,
            ts_open: 0,
            ts_close: 60,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 1000.0,
            vwap: Some(100.25),
            feature_cutoff_ts: Some(60),
        }];
        write_bars_parquet(&path, &bars).unwrap();
        let loaded = read_bar_parquet(&path).unwrap();
        assert_eq!(loaded, vec![MarketEvent::Bar(bars[0].clone())]);
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn bar_parquet_preserves_nullable_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars-nullable.parquet");
        let bars = vec![Bar {
            instrument_id: 1,
            ts_open: 0,
            ts_close: 60,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 1000.0,
            vwap: None,
            feature_cutoff_ts: None,
        }];
        write_bars_parquet(&path, &bars).unwrap();
        let loaded = read_bar_parquet(&path).unwrap();
        assert_eq!(loaded, vec![MarketEvent::Bar(bars[0].clone())]);
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn order_and_fill_parquet_preserve_nullable_strings() {
        use arrow::array::Array;
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let dir = tempfile::tempdir().unwrap();
        let orders_path = dir.path().join("orders.parquet");
        let fills_path = dir.path().join("fills.parquet");

        write_orders_parquet(
            &orders_path,
            &[OrderRequest::market(1, OrderSide::Buy, 1.0)],
        )
        .unwrap();
        write_fills_parquet(
            &fills_path,
            &[Fill {
                order_id: 1,
                instrument_id: 1,
                ts: 10,
                side: OrderSide::Buy,
                price: 100.0,
                qty: 1.0,
                fee: 0.0,
                liquidity: None,
            }],
        )
        .unwrap();

        let orders_file = File::open(&orders_path).unwrap();
        let orders_batch = ParquetRecordBatchReaderBuilder::try_new(orders_file)
            .unwrap()
            .build()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let tag_idx = orders_batch.schema().index_of("tag").unwrap();
        assert!(orders_batch.schema().field(tag_idx).is_nullable());
        assert!(orders_batch.column(tag_idx).is_null(0));

        let fills_file = File::open(&fills_path).unwrap();
        let fills_batch = ParquetRecordBatchReaderBuilder::try_new(fills_file)
            .unwrap()
            .build()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let liquidity_idx = fills_batch.schema().index_of("liquidity").unwrap();
        assert!(fills_batch.schema().field(liquidity_idx).is_nullable());
        assert!(fills_batch.column(liquidity_idx).is_null(0));
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn bar_parquet_rejects_null_required_columns() {
        use arrow::array::{ArrayRef, Float64Array, UInt32Array, UInt64Array};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars-null-required.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("instrument_id", DataType::UInt32, false),
            Field::new("ts_open", DataType::UInt64, false),
            Field::new("ts_close", DataType::UInt64, true),
            Field::new("open", DataType::Float64, false),
            Field::new("high", DataType::Float64, false),
            Field::new("low", DataType::Float64, false),
            Field::new("close", DataType::Float64, false),
            Field::new("volume", DataType::Float64, false),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt32Array::from(vec![1])),
            Arc::new(UInt64Array::from(vec![0])),
            Arc::new(UInt64Array::from(vec![None::<u64>])),
            Arc::new(Float64Array::from(vec![100.0])),
            Arc::new(Float64Array::from(vec![100.0])),
            Arc::new(Float64Array::from(vec![100.0])),
            Arc::new(Float64Array::from(vec![100.0])),
            Arc::new(Float64Array::from(vec![1.0])),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let file = File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let err = read_bar_parquet(&path).unwrap_err();
        assert!(err.to_string().contains("ts_close"));
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn bar_parquet_rejects_wrong_type_optional_cutoff() {
        use arrow::array::{ArrayRef, Float64Array, Int64Array, UInt32Array, UInt64Array};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars-bad-cutoff.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("instrument_id", DataType::UInt32, false),
            Field::new("ts_open", DataType::UInt64, false),
            Field::new("ts_close", DataType::UInt64, false),
            Field::new("open", DataType::Float64, false),
            Field::new("high", DataType::Float64, false),
            Field::new("low", DataType::Float64, false),
            Field::new("close", DataType::Float64, false),
            Field::new("volume", DataType::Float64, false),
            Field::new("feature_cutoff_ts", DataType::Int64, true),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt32Array::from(vec![1])),
            Arc::new(UInt64Array::from(vec![0])),
            Arc::new(UInt64Array::from(vec![60])),
            Arc::new(Float64Array::from(vec![100.0])),
            Arc::new(Float64Array::from(vec![101.0])),
            Arc::new(Float64Array::from(vec![99.0])),
            Arc::new(Float64Array::from(vec![100.5])),
            Arc::new(Float64Array::from(vec![1.0])),
            Arc::new(Int64Array::from(vec![60])),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let file = File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let err = read_bar_parquet(&path).unwrap_err();
        assert!(err.to_string().contains("feature_cutoff_ts"));
    }

    #[test]
    fn dbn_path_matches_databento_layout() {
        assert_eq!(
            dbn_file_path("/DATA/GLBX", "20220103"),
            "/DATA/GLBX/glbx-mdp3-20220103.mbo.dbn.zst"
        );
    }
}
