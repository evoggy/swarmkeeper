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
 *
 * Broadcast command protocol:
 *   Byte 0: Command ID
 *   Byte 1: Sub-command
 *   Byte 2+: Command-specific payload
 *
 * Command 0x01 - Synchronized function execution:
 *   Sub-command: Function index to execute
 *   Payload: uint16_t delay in ms (little-endian)
 */

#include <string.h>
#include <stdint.h>
#include <stdbool.h>

#include "app.h"

#include "FreeRTOS.h"
#include "task.h"
#include "queue.h"

#include "radiolink.h"
#include "param_logic.h"
#include "app_channel.h"

#define DEBUG_MODULE "SWARMKEEPER"
#include "debug.h"

#define LED_COLOR_WHITE 0xFFFFFF
#define LED_COLOR_OFF   0x000000

// Broadcast command IDs
#define CMD_SYNC_EXECUTE 0x01

// Synchronized execution request, passed from P2P callback to main task
typedef struct {
    uint8_t functionIndex;
    uint16_t delayMs;
} SyncExecuteRequest;

static QueueHandle_t syncExecuteQueue;

// Synchronized function table
typedef void (*SyncFunction)(void);

static paramVarId_t ledColorParam;

static void syncFunctionWhiteBlink(void) {
    paramSetInt(ledColorParam, LED_COLOR_WHITE);
    vTaskDelay(M2T(50));
    paramSetInt(ledColorParam, LED_COLOR_OFF);
}

static const SyncFunction syncFunctions[] = {
    [0] = syncFunctionWhiteBlink,
};

#define SYNC_FUNCTION_COUNT (sizeof(syncFunctions) / sizeof(syncFunctions[0]))

static void handleSyncExecute(const uint8_t *data, uint8_t size) {
    // data[0] = sub-command (function index), data[1..2] = delay ms (little-endian)
    if (size < 3) {
        return;
    }

    SyncExecuteRequest req = {
        .functionIndex = data[0],
        .delayMs = (uint16_t)(data[1] | (data[2] << 8)),
    };

    if (req.functionIndex >= SYNC_FUNCTION_COUNT) {
        return;
    }

    xQueueOverwrite(syncExecuteQueue, &req);
}

static void p2pCallback(P2PPacket *packet) {
    if (packet->size < 2) {
        return;
    }

    uint8_t commandId = packet->data[0];
    uint8_t *payload = &packet->data[1];
    uint8_t payloadSize = packet->size - 1;

    switch (commandId) {
        case CMD_SYNC_EXECUTE:
            handleSyncExecute(payload, payloadSize);
            break;
        default:
            break;
    }
}

static void handleAppChannelCommand(const uint8_t *data, uint8_t size) {
    if (size < 1) {
        return;
    }

    uint8_t commandId = data[0];
    const uint8_t *payload = &data[1];
    uint8_t payloadSize = size - 1;

    switch (commandId) {
        case CMD_SYNC_EXECUTE:
            handleSyncExecute(payload, payloadSize);
            break;
        default:
            break;
    }
}

static void broadcastTask(void *param) {
    SyncExecuteRequest req;
    while (1) {
        if (xQueueReceive(syncExecuteQueue, &req, portMAX_DELAY) == pdTRUE) {
            if (req.delayMs > 0) {
                vTaskDelay(M2T(req.delayMs));
            }
            syncFunctions[req.functionIndex]();

            // Discard any duplicate broadcasts that arrived during the delay
            while (xQueueReceive(syncExecuteQueue, &req, 0) == pdTRUE) {}
        }
    }
}

void appMain(void) {
    DEBUG_PRINT("Swarmkeeper app started\n");

    syncExecuteQueue = xQueueCreate(1, sizeof(SyncExecuteRequest));
    ledColorParam = paramGetVarId("led_deck_ctrl", "rgb888");

    p2pRegisterCB(p2pCallback);

    xTaskCreate(broadcastTask, "SWARM_BCAST", configMINIMAL_STACK_SIZE, NULL, 1, NULL);

    uint8_t appRxBuffer[APPCHANNEL_MTU];
    while (1) {
        size_t appRxLen = appchannelReceiveDataPacket(appRxBuffer, sizeof(appRxBuffer), portMAX_DELAY);
        if (appRxLen > 0) {
            //handleAppChannelCommand(appRxBuffer, (uint8_t)appRxLen);
        }
    }
}
