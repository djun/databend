// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::any::Any;
use std::sync::Arc;

use arrow_array::RecordBatch;
use common_base::base::Progress;
use common_catalog::plan::PartInfoPtr;
use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::DataBlock;
use common_expression::DataSchemaRef;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::processors::processor::Event;
use common_pipeline_core::processors::processor::ProcessorPtr;
use common_pipeline_core::processors::Processor;
use futures::StreamExt;
use icelake::io::parquet::ParquetStream;
use icelake::io::parquet::ParquetStreamBuilder;
use opendal::Operator;

use crate::partition::IcebergPartInfo;

pub struct IcebergTableSource {
    state: State,
    ctx: Arc<dyn TableContext>,
    dal: Operator,
    _scan_progress: Arc<Progress>,
    output: Arc<OutputPort>,

    /// The schema before output. Some fields might be removed when outputting.
    _source_schema: DataSchemaRef,
    /// The final output schema
    _output_schema: DataSchemaRef,
}

enum State {
    /// Read parquet file meta data
    ReadMeta(Option<PartInfoPtr>),

    /// Read data from parquet file.
    ///
    /// `Option<RecordBatch>` means there are data blocks ready for push.
    ReadData(ParquetStream, Option<RecordBatch>),

    Finish,
}

impl IcebergTableSource {
    pub fn create(
        ctx: Arc<dyn TableContext>,
        dal: Operator,
        output: Arc<OutputPort>,
        source_schema: DataSchemaRef,
        output_schema: DataSchemaRef,
    ) -> Result<ProcessorPtr> {
        let scan_progress = ctx.get_scan_progress();
        Ok(ProcessorPtr::create(Box::new(IcebergTableSource {
            ctx,
            dal,
            output,
            _scan_progress: scan_progress,
            state: State::ReadMeta(None),
            _source_schema: source_schema,
            _output_schema: output_schema,
        })))
    }
}

#[async_trait::async_trait]
impl Processor for IcebergTableSource {
    fn name(&self) -> String {
        "IcebergEngineSource".to_string()
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn event(&mut self) -> Result<Event> {
        if matches!(self.state, State::ReadMeta(None)) {
            match self.ctx.get_partition() {
                None => self.state = State::Finish,
                Some(part_info) => {
                    self.state = State::ReadMeta(Some(part_info));
                }
            }
        }

        if self.output.is_finished() {
            return Ok(Event::Finished);
        }

        if !self.output.can_push() {
            return Ok(Event::NeedConsume);
        }

        if matches!(self.state, State::ReadData(_, _)) {
            if let State::ReadData(ps, mut data) = std::mem::replace(&mut self.state, State::Finish)
            {
                if let Some(arrow_block) = data.take() {
                    let (data_block, _) =
                        DataBlock::from_record_batch(&arrow_block).map_err(|err| {
                            ErrorCode::ReadTableDataError(format!(
                                "Cannot convert arrow record batch to data block: {err:?}"
                            ))
                        })?;
                    self.output.push_data(Ok(data_block));
                }

                // Let's fetch more data.
                self.state = State::ReadData(ps, None);

                return Ok(Event::Async);
            }
        }

        match self.state {
            State::Finish => {
                self.output.finish();
                Ok(Event::Finished)
            }
            State::ReadMeta(_) => Ok(Event::Async),
            State::ReadData(_, _) => Ok(Event::Async),
        }
    }

    fn process(&mut self) -> Result<()> {
        Err(ErrorCode::Internal(
            "It's a bug for IcebergTableSource to go into Event::Sync.",
        ))
    }

    #[async_backtrace::framed]
    async fn async_process(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::Finish) {
            State::ReadMeta(Some(part)) => {
                let part = IcebergPartInfo::from_part(&part)?;
                let r = self.dal.reader(&part.path).await?;
                let s = ParquetStreamBuilder::new(r)
                    .build()
                    .await
                    .map_err(parse_icelake_error)?;
                self.state = State::ReadData(s, None);
                Ok(())
            }
            State::ReadData(mut stream, None) => match stream.next().await {
                None => {
                    self.state = State::ReadMeta(None);
                    Ok(())
                }
                Some(data) => {
                    let data = data.map_err(parse_icelake_error)?;
                    self.state = State::ReadData(stream, Some(data));
                    Ok(())
                }
            },
            _ => Err(ErrorCode::Internal(
                "It's a bug for IcebergTableSource to async_process current state.",
            )),
        }
    }
}

fn parse_icelake_error(err: icelake::Error) -> ErrorCode {
    ErrorCode::ReadTableDataError(format!("icelake operation failed: {:?}", err))
}
