// Copyright 2018-2022 Cargill Incorporated
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

//! Provides the "list service" operation for the `DieselLifecycleStore`.
use std::convert::TryFrom;

use diesel::prelude::*;

use crate::runtime::service::lifecycle_executor::store::{
    diesel::{
        models::{ServiceLifecycleArgumentModel, ServiceLifecycleStatusModel},
        schema::{service_lifecycle_argument, service_lifecycle_status},
    },
    error::LifecycleStoreError,
    LifecycleCommand, LifecycleService, LifecycleServiceBuilder, LifecycleStatus,
};
use crate::service::{CircuitId, FullyQualifiedServiceId, ServiceId, ServiceType};

use super::LifecycleStoreOperations;

pub(in crate::runtime::service::lifecycle_executor::store::diesel) trait LifecycleStoreListServiceOperation
{
    fn list_service(
        &self,
        status: &LifecycleStatus,
    ) -> Result<Vec<LifecycleService>, LifecycleStoreError>;
}

impl<'a, C> LifecycleStoreListServiceOperation for LifecycleStoreOperations<'a, C>
where
    C: diesel::Connection,
    String: diesel::deserialize::FromSql<diesel::sql_types::Text, C::Backend>,
    i32: diesel::deserialize::FromSql<diesel::sql_types::Integer, C::Backend>,
{
    fn list_service(
        &self,
        status: &LifecycleStatus,
    ) -> Result<Vec<LifecycleService>, LifecycleStoreError> {
        self.conn.transaction::<Vec<LifecycleService>, _, _>(|| {
            // Fetch the `service` entry with the matching `service_id`.
            // return None if the `service` does not exist
            let services: Vec<ServiceLifecycleStatusModel> = service_lifecycle_status::table
                .filter(service_lifecycle_status::status.eq(&String::from(status)))
                .load::<ServiceLifecycleStatusModel>(self.conn)?;

            let mut return_services = Vec::new();
            for service in services {
                // Collect the `service_arguments` entries with the associated `circuit_id` found
                // in the `service` entry previously fetched and the provided `service_id`.
                let arguments: Vec<(String, String)> = service_lifecycle_argument::table
                    .filter(service_lifecycle_argument::circuit_id.eq(service.circuit_id.as_str()))
                    .filter(service_lifecycle_argument::service_id.eq(service.service_id.as_str()))
                    .order(service_lifecycle_argument::position)
                    .load::<ServiceLifecycleArgumentModel>(self.conn)?
                    .iter()
                    .map(|arg| (arg.key.to_string(), arg.value.to_string()))
                    .collect();

                let return_service = LifecycleServiceBuilder::new()
                    .with_service_id(&FullyQualifiedServiceId::new(
                        CircuitId::new(service.circuit_id.as_str())?,
                        ServiceId::new(service.service_id.as_str())?,
                    ))
                    .with_service_type(&ServiceType::new(service.service_type)?)
                    .with_arguments(&arguments)
                    .with_command(&LifecycleCommand::try_from(service.command.as_str())?)
                    .with_status(&LifecycleStatus::try_from(service.status.as_str())?)
                    .build()
                    .map_err(LifecycleStoreError::InvalidState)?;

                return_services.push(return_service);
            }

            Ok(return_services)
        })
    }
}
