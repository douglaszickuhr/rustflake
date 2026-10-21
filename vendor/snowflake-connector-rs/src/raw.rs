//! Unstable diagnostics for rustflake: a query's raw result description, to compare result formats
//! (JSON vs Arrow) before the connector decodes Arrow.

use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use crate::statement::wire::response::resolve_download_headers;
use crate::statement::{QueryControl, StatementExecutor, builder::into_statement_parts};
use crate::{QueryOptions, Result, Session};

#[derive(Debug, Clone)]
pub struct RawChunk {
    pub url: String,
    pub row_count: i64,
    pub compressed_size: i64,
    pub uncompressed_size: i64,
}

#[derive(Debug, Clone)]
pub struct RawResult {
    pub format: Option<String>,
    /// First part inline: JSON rowset bytes, or the base64 Arrow stream's length.
    pub first_part_bytes: usize,
    pub chunks: Vec<RawChunk>,
    /// Headers to send when downloading the chunks (S3 encryption key).
    pub download_headers: HashMap<String, String>,
}

impl Session {
    #[doc(hidden)]
    pub async fn raw_query(&self, sql: String) -> Result<RawResult> {
        let parts = into_statement_parts(sql)?;
        let settings = self.shared.query.resolve_options(QueryOptions::default());
        let control = QueryControl::new(Arc::from(Uuid::new_v4().to_string()));
        let response = StatementExecutor::new(self, settings)
            .execute_raw(parts, control)
            .await?;
        let Some(d) = response.data else {
            return Ok(RawResult {
                format: None,
                first_part_bytes: 0,
                chunks: Vec::new(),
                download_headers: HashMap::new(),
            });
        };
        let headers =
            resolve_download_headers(&d.qrmk, &d.chunk_headers).map_err(crate::Error::from)?;
        Ok(RawResult {
            format: d.query_result_format,
            first_part_bytes: d
                .row_set_base64
                .as_ref()
                .map(String::len)
                .or(d.row_set_bytes.as_ref().map(|b| b.len()))
                .unwrap_or(0),
            chunks: d
                .chunks
                .unwrap_or_default()
                .into_iter()
                .map(|c| RawChunk {
                    url: c.url,
                    row_count: c.row_count,
                    compressed_size: c.compressed_size,
                    uncompressed_size: c.uncompressed_size,
                })
                .collect(),
            download_headers: headers
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
                .collect(),
        })
    }
}
