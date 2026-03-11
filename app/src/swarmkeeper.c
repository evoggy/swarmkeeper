/**
 * Swarmkeeper - Crazyflie Swarm Management
 *
 * Copyright (C) 2025 Bitcraze AB
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * Alternatively, this file may be used under the terms of the MIT license.
 *
 * swarmkeeper.c - App layer application for swarm functionality
 */

#include <string.h>
#include <stdint.h>
#include <stdbool.h>

#include "app.h"

#include "FreeRTOS.h"
#include "task.h"

#define DEBUG_MODULE "SWARMKEEPER"
#include "debug.h"

void appMain(void) {
    DEBUG_PRINT("Swarmkeeper app started\n");

    while (1) {
        vTaskDelay(M2T(1000));
    }
}
