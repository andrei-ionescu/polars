use crate::aggregations::{apply_aggregations, ScanAggregation};
use crate::mmap::{MmapBytesReader, ReaderBytes};
use crate::parquet::mmap;
use crate::parquet::mmap::mmap_columns;
use crate::parquet::predicates::collect_statistics;
use crate::predicates::{apply_predicate, arrow_schema_to_empty_df, PhysicalIoExpr};
use crate::utils::apply_projection;
use crate::RowCount;
use arrow::array::new_empty_array;
use arrow::io::parquet::read;
use arrow::io::parquet::read::{ArrayIter, FileMetaData};
use polars_core::prelude::*;
use polars_core::utils::accumulate_dataframes_vertical;
use polars_core::POOL;
use rayon::prelude::*;
use std::borrow::Cow;
use std::convert::TryFrom;
use std::ops::Deref;
use std::sync::Arc;

fn array_iter_to_series(
    iter: ArrayIter,
    field: &ArrowField,
    num_rows: Option<usize>,
) -> Result<Series> {
    let mut total_count = 0;
    let chunks = match num_rows {
        None => iter.collect::<arrow::error::Result<Vec<_>>>()?,
        Some(n) => {
            let mut out = Vec::with_capacity(2);

            for arr in iter {
                let arr = arr?;
                let len = arr.len();
                out.push(arr);

                total_count += len;
                if total_count >= n {
                    break;
                }
            }
            out
        }
    };
    if chunks.is_empty() {
        let arr = new_empty_array(field.data_type.clone());
        Series::try_from((field.name.as_str(), arr))
    } else {
        Series::try_from((field.name.as_str(), chunks))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn read_parquet<R: MmapBytesReader>(
    mut reader: R,
    limit: usize,
    projection: Option<&[usize]>,
    schema: &ArrowSchema,
    metadata: Option<FileMetaData>,
    predicate: Option<Arc<dyn PhysicalIoExpr>>,
    aggregate: Option<&[ScanAggregation]>,
    mut parallel: bool,
    row_count: Option<RowCount>,
) -> Result<DataFrame> {
    let file_metadata = metadata
        .map(Ok)
        .unwrap_or_else(|| read::read_metadata(&mut reader))?;
    let row_group_len = file_metadata.row_groups.len();

    let projection = projection
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Owned((0usize..schema.fields.len()).collect::<Vec<_>>()));

    if projection.len() == 1 {
        parallel = false;
    }

    let mut dfs = Vec::with_capacity(row_group_len);

    let mut remaining_rows = limit;

    let reader = ReaderBytes::from(&reader);
    let bytes = reader.deref();

    let mut previous_row_count = 0;
    for rg in 0..row_group_len {
        let md = &file_metadata.row_groups[rg];
        let current_row_count = md.num_rows() as IdxSize;
        if let Some(pred) = &predicate {
            if let Some(pred) = pred.as_stats_evaluator() {
                if let Some(stats) = collect_statistics(&file_metadata.row_groups, schema)? {
                    let should_read = pred.should_read(&stats);
                    // a parquet file may not have statistics of all columns
                    if matches!(should_read, Ok(false)) {
                        previous_row_count += current_row_count;
                        continue;
                    } else if !matches!(should_read, Err(PolarsError::NotFound(_))) {
                        let _ = should_read?;
                    }
                }
            }
        }

        // test we don't read the parquet file if this env var is set
        #[cfg(debug_assertions)]
        {
            assert!(std::env::var("POLARS_PANIC_IF_PARQUET_PARSED").is_err())
        }

        let chunk_size = md.num_rows() as usize;
        let columns = if parallel {
            POOL.install(|| {
                projection
                    .par_iter()
                    .map(|column_i| {
                        let field = &schema.fields[*column_i];
                        let columns = mmap_columns(bytes, md.columns(), &field.name);
                        let iter = mmap::to_deserializer(
                            columns,
                            field.clone(),
                            remaining_rows,
                            Some(chunk_size),
                        )?;

                        if remaining_rows < md.num_rows() {
                            array_iter_to_series(iter, field, Some(remaining_rows))
                        } else {
                            array_iter_to_series(iter, field, None)
                        }
                    })
                    .collect::<Result<Vec<_>>>()
            })?
        } else {
            projection
                .iter()
                .map(|column_i| {
                    let field = &schema.fields[*column_i];
                    let columns = mmap_columns(bytes, md.columns(), &field.name);
                    let iter = mmap::to_deserializer(
                        columns,
                        field.clone(),
                        remaining_rows,
                        Some(chunk_size),
                    )?;

                    if remaining_rows < md.num_rows() {
                        array_iter_to_series(iter, field, Some(remaining_rows))
                    } else {
                        array_iter_to_series(iter, field, None)
                    }
                })
                .collect::<Result<Vec<_>>>()?
        };

        remaining_rows =
            remaining_rows.saturating_sub(file_metadata.row_groups[rg].num_rows() as usize);

        let mut df = DataFrame::new_no_checks(columns);
        if let Some(rc) = &row_count {
            df.with_row_count_mut(&rc.name, Some(previous_row_count + rc.offset));
        }

        apply_predicate(&mut df, predicate.as_deref())?;
        apply_aggregations(&mut df, aggregate)?;

        previous_row_count += current_row_count;
        dfs.push(df)
    }

    if dfs.is_empty() {
        let schema = if let Cow::Borrowed(_) = projection {
            Cow::Owned(apply_projection(schema, &projection))
        } else {
            Cow::Borrowed(schema)
        };
        Ok(arrow_schema_to_empty_df(&schema))
    } else {
        let mut df = accumulate_dataframes_vertical(dfs.into_iter())?;
        apply_aggregations(&mut df, aggregate)?;
        Ok(df.slice(0, limit))
    }
}
