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

use std::sync::Arc;
use std::time::Instant;

use common_catalog::plan::StageTableInfo;
use common_catalog::table::AppendMode;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::infer_table_schema;
use common_expression::BlockThresholds;
use common_expression::DataField;
use common_expression::DataSchemaRef;
use common_expression::DataSchemaRefExt;
use common_meta_app::principal::StageInfo;
use common_pipeline_core::Pipeline;
use common_sql::executor::table_read_plan::ToReadDataSourcePlan;
use common_sql::executor::DistributedCopyIntoTable;
use common_sql::executor::Exchange;
use common_sql::executor::FragmentKind;
use common_sql::executor::PhysicalPlan;
use common_sql::plans::CopyIntoTableMode;
use common_sql::plans::CopyIntoTablePlan;
use common_storage::StageFileInfo;
use common_storage::StageFilesInfo;
use common_storages_stage::StageTable;
use tracing::info;

use crate::interpreters::common::check_deduplicate_label;
use crate::interpreters::Interpreter;
use crate::interpreters::SelectInterpreter;
use crate::pipelines::builders::build_append2table_pipeline;
use crate::pipelines::builders::build_append_data_with_finish_pipeline;
use crate::pipelines::builders::build_upsert_copied_files_to_meta_req;
use crate::pipelines::builders::try_purge_files;
use crate::pipelines::builders::CopyPlanParam;
use crate::pipelines::PipelineBuildResult;
use crate::schedulers::build_distributed_pipeline;
use crate::sessions::QueryContext;
use crate::sessions::TableContext;
use crate::sql::plans::CopyPlan;
use crate::sql::plans::Plan;

pub struct CopyInterpreter {
    ctx: Arc<QueryContext>,
    plan: CopyPlan,
}

impl CopyInterpreter {
    /// Create a CopyInterpreter with context and [`CopyPlan`].
    pub fn try_create(ctx: Arc<QueryContext>, plan: CopyPlan) -> Result<Self> {
        Ok(CopyInterpreter { ctx, plan })
    }

    #[async_backtrace::framed]
    async fn build_query(&self, query: &Plan) -> Result<(PipelineBuildResult, DataSchemaRef)> {
        let (s_expr, metadata, bind_context, formatted_ast) = match query {
            Plan::Query {
                s_expr,
                metadata,
                bind_context,
                formatted_ast,
                ..
            } => (s_expr, metadata, bind_context, formatted_ast),
            v => unreachable!("Input plan must be Query, but it's {}", v),
        };

        let select_interpreter = SelectInterpreter::try_create(
            self.ctx.clone(),
            *(bind_context.clone()),
            *s_expr.clone(),
            metadata.clone(),
            formatted_ast.clone(),
            false,
        )?;

        // Building data schema from bind_context columns
        // TODO(leiyskey): Extract the following logic as new API of BindContext.
        let fields = bind_context
            .columns
            .iter()
            .map(|column_binding| {
                DataField::new(
                    &column_binding.column_name,
                    *column_binding.data_type.clone(),
                )
            })
            .collect();
        let data_schema = DataSchemaRefExt::create(fields);
        let plan = select_interpreter.build_physical_plan().await?;
        let build_res = select_interpreter.build_pipeline(plan).await?;
        Ok((build_res, data_schema))
    }

    /// Build a pipeline for local copy into stage.
    #[async_backtrace::framed]
    async fn build_local_copy_into_stage_pipeline(
        &self,
        stage: &StageInfo,
        path: &str,
        query: &Plan,
    ) -> Result<PipelineBuildResult> {
        let (mut build_res, data_schema) = self.build_query(query).await?;
        let table_schema = infer_table_schema(&data_schema)?;
        let stage_table_info = StageTableInfo {
            schema: table_schema,
            stage_info: stage.clone(),
            files_info: StageFilesInfo {
                path: path.to_string(),
                files: None,
                pattern: None,
            },
            files_to_copy: None,
            is_select: false,
        };
        let table = StageTable::try_create(stage_table_info)?;
        build_append2table_pipeline(
            self.ctx.clone(),
            &mut build_res.main_pipeline,
            table,
            data_schema,
            None,
            false,
            AppendMode::Normal,
        )?;
        Ok(build_res)
    }

    #[async_backtrace::framed]
    async fn try_purge_files(
        ctx: Arc<QueryContext>,
        stage_info: &StageInfo,
        stage_file_infos: &[StageFileInfo],
    ) {
        let purge_start = Instant::now();
        let num_copied_files = stage_file_infos.len();

        // Status.
        {
            let status = format!("begin to purge files:{}", num_copied_files);
            ctx.set_status_info(&status);
            info!(status);
        }

        try_purge_files(ctx.clone(), stage_info, stage_file_infos).await;

        // Status.
        info!(
            "end to purge files:{}, elapsed:{}",
            num_copied_files,
            purge_start.elapsed().as_secs()
        );
    }

    fn set_status(&self, status: &str) {
        self.ctx.set_status_info(status);
        info!(status);
    }

    #[async_backtrace::framed]
    async fn try_transform_copy_plan_from_local_to_distributed(
        &self,
        plan: &CopyIntoTablePlan,
    ) -> Result<Option<DistributedCopyIntoTable>> {
        let ctx = self.ctx.clone();
        let to_table = ctx
            .get_table(&plan.catalog_name, &plan.database_name, &plan.table_name)
            .await?;
        let table_ctx: Arc<dyn TableContext> = self.ctx.clone();
        let files = plan.collect_files(&table_ctx).await?;
        if files.is_empty() {
            return Ok(None);
        }
        let mut stage_table_info = plan.stage_table_info.clone();
        stage_table_info.files_to_copy = Some(files.clone());
        let stage_table = StageTable::try_create(stage_table_info.clone())?;
        let read_source_plan = {
            stage_table
                .read_plan_with_catalog(
                    self.ctx.clone(),
                    plan.catalog_name.to_string(),
                    None,
                    None,
                    false,
                )
                .await?
        };

        if read_source_plan.parts.len() <= 1 {
            return Ok(None);
        }
        Ok(Some(DistributedCopyIntoTable {
            // TODO(leiysky): we reuse the id of exchange here,
            // which is not correct. We should generate a new id for insert.
            plan_id: 0,
            catalog_name: plan.catalog_name.clone(),
            database_name: plan.database_name.clone(),
            table_name: plan.table_name.clone(),
            required_values_schema: plan.required_values_schema.clone(),
            values_consts: plan.values_consts.clone(),
            required_source_schema: plan.required_source_schema.clone(),
            write_mode: plan.write_mode,
            validation_mode: plan.validation_mode.clone(),
            force: plan.force,
            stage_table_info: plan.stage_table_info.clone(),
            source: Box::new(read_source_plan),
            thresholds: to_table.get_block_thresholds(),
            files,
            table_info: to_table.get_table_info().clone(),
            local_node_id: self.ctx.get_cluster().local_id.clone(),
        }))
    }

    #[async_backtrace::framed]
    async fn build_read_stage_table_data_pipeline(
        &self,
        pipeline: &mut Pipeline,
        plan: &CopyIntoTablePlan,
        block_thresholds: BlockThresholds,
        files: Vec<StageFileInfo>,
    ) -> Result<()> {
        let ctx = self.ctx.clone();
        let table_ctx: Arc<dyn TableContext> = ctx.clone();

        self.set_status("begin to read stage source plan");

        let mut stage_table_info = plan.stage_table_info.clone();
        stage_table_info.files_to_copy = Some(files.clone());
        let stage_table = StageTable::try_create(stage_table_info.clone())?;
        let read_source_plan = {
            stage_table
                .read_plan_with_catalog(
                    ctx.clone(),
                    plan.catalog_name.to_string(),
                    None,
                    None,
                    false,
                )
                .await?
        };

        self.set_status(&format!(
            "begin to read stage table data, parts:{}",
            read_source_plan.parts.len()
        ));

        stage_table.set_block_thresholds(block_thresholds);
        stage_table.read_data(table_ctx, &read_source_plan, pipeline)?;
        Ok(())
    }

    /// Build a pipeline to copy data from local.
    #[async_backtrace::framed]
    async fn build_local_copy_into_table_pipeline(
        &self,
        plan: &CopyIntoTablePlan,
    ) -> Result<PipelineBuildResult> {
        let start = Instant::now();
        let ctx = self.ctx.clone();
        let to_table = ctx
            .get_table(&plan.catalog_name, &plan.database_name, &plan.table_name)
            .await?;

        let (mut build_res, source_schema, files) = if let Some(query) = &plan.query {
            let (build_res, source_schema) = self.build_query(query).await?;
            (
                build_res,
                source_schema,
                plan.stage_table_info
                    .files_to_copy
                    .clone()
                    .ok_or(ErrorCode::Internal("files_to_copy should not be None"))?,
            )
        } else {
            let table_ctx: Arc<dyn TableContext> = self.ctx.clone();
            let files = plan.collect_files(&table_ctx).await?;
            let mut build_res = PipelineBuildResult::create();
            if files.is_empty() {
                return Ok(build_res);
            }
            self.build_read_stage_table_data_pipeline(
                &mut build_res.main_pipeline,
                plan,
                to_table.get_block_thresholds(),
                files.clone(),
            )
            .await?;
            (build_res, plan.required_source_schema.clone(), files)
        };

        build_append_data_with_finish_pipeline(
            ctx,
            &mut build_res.main_pipeline,
            source_schema,
            CopyPlanParam::CopyIntoTablePlanOption(plan.clone()),
            to_table,
            files,
            start,
            true,
        )?;
        Ok(build_res)
    }

    /// Build a pipeline to copy data into table for distributed.
    #[async_backtrace::framed]
    async fn build_distributed_copy_into_table_pipeline(
        &self,
        distributed_plan: &DistributedCopyIntoTable,
    ) -> Result<PipelineBuildResult> {
        // add exchange plan node to enable distributed
        // TODO(leiysky): we reuse the id of exchange here,
        // which is not correct. We should generate a new id for insert.
        let exchange_plan = PhysicalPlan::Exchange(Exchange {
            plan_id: 0,
            input: Box::new(PhysicalPlan::DistributedCopyIntoTable(Box::new(
                distributed_plan.clone(),
            ))),
            kind: FragmentKind::Merge,
            keys: Vec::new(),
        });
        let mut build_res = build_distributed_pipeline(&self.ctx, &exchange_plan, false).await?;

        let catalog = self.ctx.get_catalog(&distributed_plan.catalog_name)?;
        let to_table = catalog.get_table_by_info(&distributed_plan.table_info)?;
        let copied_files = build_upsert_copied_files_to_meta_req(
            self.ctx.clone(),
            to_table.clone(),
            distributed_plan.stage_table_info.stage_info.clone(),
            distributed_plan.files.clone(),
            distributed_plan.force,
        )?;
        let mut overwrite_ = false;
        if let CopyIntoTableMode::Insert { overwrite } = distributed_plan.write_mode {
            overwrite_ = overwrite;
        }
        to_table.commit_insertion(
            self.ctx.clone(),
            &mut build_res.main_pipeline,
            copied_files,
            overwrite_,
        )?;

        Ok(build_res)
    }
}

#[async_trait::async_trait]
impl Interpreter for CopyInterpreter {
    fn name(&self) -> &str {
        "CopyInterpreterV2"
    }

    #[tracing::instrument(level = "debug", name = "copy_interpreter_execute_v2", skip(self), fields(ctx.id = self.ctx.get_id().as_str()))]
    #[async_backtrace::framed]
    async fn execute2(&self) -> Result<PipelineBuildResult> {
        if check_deduplicate_label(self.ctx.clone()).await? {
            return Ok(PipelineBuildResult::create());
        }

        match &self.plan {
            CopyPlan::IntoTable(plan) => {
                if plan.enable_distributed {
                    let distributed_plan_op = self
                        .try_transform_copy_plan_from_local_to_distributed(plan)
                        .await?;
                    if let Some(distributed_plan) = distributed_plan_op {
                        self.build_distributed_copy_into_table_pipeline(&distributed_plan)
                            .await
                    } else {
                        self.build_local_copy_into_table_pipeline(plan).await
                    }
                } else {
                    self.build_local_copy_into_table_pipeline(plan).await
                }
            }
            CopyPlan::IntoStage {
                stage, from, path, ..
            } => {
                self.build_local_copy_into_stage_pipeline(stage, path, from)
                    .await
            }
            CopyPlan::NoFileToCopy => Ok(PipelineBuildResult::create()),
        }
    }
}
