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

use log::info;
use splinter::{
    error::InternalError,
    service::{FullyQualifiedServiceId, MessageHandler, MessageSender},
};

use super::ScabbardMessage;

#[derive(Default)]
pub struct ScabbardMessageHandler {}

impl ScabbardMessageHandler {
    pub fn new() -> Self {
        Self {}
    }
}

impl MessageHandler for ScabbardMessageHandler {
    type Message = ScabbardMessage;

    fn handle_message(
        &mut self,
        _sender: &dyn MessageSender<Self::Message>,
        to_service: FullyQualifiedServiceId,
        from_service: FullyQualifiedServiceId,
        _message: Self::Message,
    ) -> Result<(), InternalError> {
        info!(
            "handling scabbard message, to: {} from: {}",
            to_service, from_service
        );
        Ok(())
    }
}
