// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use log::{debug, warn};

use crate::scheduler_server::event::SchedulerServerEvent;
use crate::scheduler_server::ExecutorsClient;
use crate::state::task_scheduler::TaskScheduler;
use crate::state::SchedulerState;
use ballista_core::error::{BallistaError, Result};
use ballista_core::event_loop::EventAction;
use ballista_core::serde::protobuf::{LaunchTaskParams, TaskDefinition};
use ballista_core::serde::scheduler::ExecutorDataChange;
use ballista_core::serde::AsExecutionPlan;
use datafusion_proto::logical_plan::AsLogicalPlan;

pub(crate) struct SchedulerServerEventAction<
    T: 'static + AsLogicalPlan,
    U: 'static + AsExecutionPlan,
> {
    state: Arc<SchedulerState<T, U>>,
    executors_client: ExecutorsClient,
}

impl<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan>
    SchedulerServerEventAction<T, U>
{
    pub fn new(
        state: Arc<SchedulerState<T, U>>,
        executors_client: ExecutorsClient,
    ) -> Self {
        Self {
            state,
            executors_client,
        }
    }

    #[allow(unused_variables)]
    async fn offer_resources(&self, n: u32) -> Result<Option<SchedulerServerEvent>> {
        let mut available_executors =
            self.state.executor_manager.get_available_executors_data();
        // In case of there's no enough resources, reschedule the tasks of the job
        if available_executors.is_empty() {
            // TODO Maybe it's better to use an exclusive runtime for this kind task scheduling
            warn!("Not enough available executors for task running");
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Ok(Some(SchedulerServerEvent::ReviveOffers(1)));
        }

        let mut executors_data_change: Vec<ExecutorDataChange> = available_executors
            .iter()
            .map(|executor_data| ExecutorDataChange {
                executor_id: executor_data.executor_id.clone(),
                task_slots: executor_data.available_task_slots as i32,
            })
            .collect();

        let (tasks_assigment, num_tasks) = self
            .state
            .fetch_schedulable_tasks(&mut available_executors, n)
            .await?;
        for (data_change, data) in executors_data_change
            .iter_mut()
            .zip(available_executors.iter())
        {
            data_change.task_slots =
                data.available_task_slots as i32 - data_change.task_slots;
        }

        #[cfg(not(test))]
        if num_tasks > 0 {
            self.launch_tasks(&executors_data_change, tasks_assigment)
                .await?;
        }

        Ok(None)
    }

    #[allow(dead_code)]
    async fn launch_tasks(
        &self,
        executors: &[ExecutorDataChange],
        tasks_assigment: Vec<Vec<TaskDefinition>>,
    ) -> Result<()> {
        for (idx_executor, tasks) in tasks_assigment.into_iter().enumerate() {
            if !tasks.is_empty() {
                let executor_data_change = &executors[idx_executor];
                debug!(
                    "Start to launch tasks {:?} to executor {:?}",
                    tasks
                        .iter()
                        .map(|task| {
                            if let Some(task_id) = task.task_id.as_ref() {
                                format!(
                                    "{}/{}/{}",
                                    task_id.job_id,
                                    task_id.stage_id,
                                    task_id.partition_id
                                )
                            } else {
                                "".to_string()
                            }
                        })
                        .collect::<Vec<String>>(),
                    executor_data_change.executor_id
                );
                let mut client = {
                    let clients = self.executors_client.read().await;
                    clients
                        .get(&executor_data_change.executor_id)
                        .unwrap()
                        .clone()
                };
                // TODO check whether launching task is successful or not
                client.launch_task(LaunchTaskParams { task: tasks }).await?;
                self.state
                    .executor_manager
                    .update_executor_data(executor_data_change);
            } else {
                // Since the task assignment policy is round robin,
                // if find tasks for one executor is empty, just break fast
                break;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan>
    EventAction<SchedulerServerEvent> for SchedulerServerEventAction<T, U>
{
    // TODO
    fn on_start(&self) {}

    // TODO
    fn on_stop(&self) {}

    async fn on_receive(
        &self,
        event: SchedulerServerEvent,
    ) -> Result<Option<SchedulerServerEvent>> {
        match event {
            SchedulerServerEvent::ReviveOffers(n) => self.offer_resources(n).await,
        }
    }

    // TODO
    fn on_error(&self, _error: BallistaError) {}
}
