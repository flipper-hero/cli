#include <furi.h>
#include <furi_hal.h>
#include <expansion/expansion.h>
#include <gui/gui.h>
#include <gui/view_port.h>
#include <input/input.h>
#include <rpc/rpc_app.h>

#include <stdio.h>
#include <string.h>

#define UART_BAUD 115200
#define RX_CAPACITY 8192
#define RX_CHUNK 192

typedef enum {
    BridgeEventExit,
    BridgeEventCommand,
} BridgeEventType;

typedef struct {
    BridgeEventType type;
    char command[40];
    size_t length;
} BridgeEvent;

typedef struct {
    RpcAppSystem* rpc;
    FuriMutex* rpc_mutex;
    FuriMessageQueue* events;
    FuriStreamBuffer* rx;
    FuriHalSerialHandle* serial;
    Gui* gui;
    ViewPort* view_port;
    Expansion* expansion;
    bool otg_was_enabled;
    volatile bool exit_requested;
    volatile bool rx_overflow;
} MarauderBridge;

static bool command_allowed(const uint8_t* data, size_t size) {
    static const char* const commands[] = {
        "help", "info", "list -a", "list -c", "list -s", "scanap", "scansta", "stopscan",
        "sniffbeacon", "sniffprobe", "sniffdeauth", "sniffpmkid",
        "attack -t deauth", "attack -t probe", "attack -t beacon -r", "attack -t rickroll"};
    for(size_t i = 0; i < COUNT_OF(commands); i++) {
        if(size == strlen(commands[i]) && memcmp(data, commands[i], size) == 0) return true;
    }
    static const char prefix[] = "select -a ";
    if(size <= sizeof(prefix) - 1 || size > sizeof(prefix) + 1 ||
       (memcmp(data, prefix, sizeof(prefix) - 1) != 0 &&
        memcmp(data, "select -s ", sizeof(prefix) - 1) != 0)) return false;
    for(size_t i = sizeof(prefix) - 1; i < size; i++) {
        if(data[i] < '0' || data[i] > '9') return false;
    }
    return true;
}

static void queue_exit(MarauderBridge* bridge) {
    bridge->exit_requested = true;
    const BridgeEvent event = {.type = BridgeEventExit};
    furi_message_queue_put(bridge->events, &event, 0);
}

static void rpc_callback(const RpcAppSystemEvent* event, void* context) {
    MarauderBridge* bridge = context;
    if(event->type == RpcAppEventTypeSessionClose) {
        furi_mutex_acquire(bridge->rpc_mutex, FuriWaitForever);
        rpc_system_app_set_callback(bridge->rpc, NULL, NULL);
        bridge->rpc = NULL;
        furi_mutex_release(bridge->rpc_mutex);
        queue_exit(bridge);
    } else if(event->type == RpcAppEventTypeAppExit) {
        rpc_system_app_confirm(bridge->rpc, true);
        queue_exit(bridge);
    } else if(event->type == RpcAppEventTypeDataExchange) {
        const uint8_t* data = event->data.bytes.ptr;
        const size_t size = event->data.bytes.size;
        bool accepted = event->data.type == RpcAppSystemEventDataTypeBytes &&
                        command_allowed(data, size);
        if(accepted) {
            BridgeEvent command = {.type = BridgeEventCommand, .length = size};
            memcpy(command.command, data, size);
            accepted = furi_message_queue_put(bridge->events, &command, 0) == FuriStatusOk;
        }
        rpc_system_app_confirm(bridge->rpc, accepted);
    } else {
        rpc_system_app_confirm(bridge->rpc, false);
    }
}

static void uart_callback(FuriHalSerialHandle* handle, FuriHalSerialRxEvent event, void* context) {
    MarauderBridge* bridge = context;
    if(event == FuriHalSerialRxEventData) {
        const uint8_t byte = furi_hal_serial_async_rx(handle);
        if(furi_stream_buffer_send(bridge->rx, &byte, 1, 0) != 1) bridge->rx_overflow = true;
    }
}

static void draw_callback(Canvas* canvas, void* context) {
    UNUSED(context);
    canvas_clear(canvas);
    canvas_set_font(canvas, FontPrimary);
    canvas_draw_str(canvas, 2, 13, "Marauder Bridge");
    canvas_set_font(canvas, FontSecondary);
    canvas_draw_str(canvas, 2, 31, "USB / BLE RPC connected");
    canvas_draw_str(canvas, 2, 47, "Back to exit");
}

static void input_callback(InputEvent* input, void* context) {
    MarauderBridge* bridge = context;
    if(input->key == InputKeyBack && input->type == InputTypeShort) queue_exit(bridge);
}

int32_t marauder_bridge_app(void* args) {
    unsigned long rpc_address = 0;
    if(!args || sscanf(args, "RPC %lX", &rpc_address) != 1 || !rpc_address) return 1;

    MarauderBridge* bridge = malloc(sizeof(MarauderBridge));
    bridge->rpc = (RpcAppSystem*)rpc_address;
    bridge->rpc_mutex = furi_mutex_alloc(FuriMutexTypeNormal);
    bridge->events = furi_message_queue_alloc(8, sizeof(BridgeEvent));
    bridge->rx = furi_stream_buffer_alloc(RX_CAPACITY, 1);
    bridge->exit_requested = false;
    bridge->rx_overflow = false;

    bridge->expansion = furi_record_open(RECORD_EXPANSION);
    expansion_disable(bridge->expansion);
    bridge->otg_was_enabled = furi_hal_power_is_otg_enabled();
    if(!bridge->otg_was_enabled) {
        for(size_t attempt = 0; attempt < 5 && !furi_hal_power_is_otg_enabled(); attempt++) {
            furi_hal_power_enable_otg();
            furi_delay_ms(10);
        }
    }
    furi_delay_ms(200);

    bridge->serial = furi_hal_serial_control_acquire(FuriHalSerialIdUsart);
    if(!bridge->serial) {
        if(!bridge->otg_was_enabled) furi_hal_power_disable_otg();
        expansion_enable(bridge->expansion);
        furi_record_close(RECORD_EXPANSION);
        furi_stream_buffer_free(bridge->rx);
        furi_message_queue_free(bridge->events);
        furi_mutex_free(bridge->rpc_mutex);
        free(bridge);
        return 2;
    }
    furi_hal_serial_init(bridge->serial, UART_BAUD);
    furi_hal_serial_async_rx_start(bridge->serial, uart_callback, bridge, false);

    bridge->gui = furi_record_open(RECORD_GUI);
    bridge->view_port = view_port_alloc();
    view_port_draw_callback_set(bridge->view_port, draw_callback, bridge);
    view_port_input_callback_set(bridge->view_port, input_callback, bridge);
    gui_add_view_port(bridge->gui, bridge->view_port, GuiLayerFullscreen);

    rpc_system_app_set_callback(bridge->rpc, rpc_callback, bridge);
    rpc_system_app_send_started(bridge->rpc);

    bool running = true;
    bool scan_active = false;
    uint8_t bytes[RX_CHUNK];
    while(running && !bridge->exit_requested) {
        BridgeEvent event;
        if(furi_message_queue_get(bridge->events, &event, 10) == FuriStatusOk) {
            if(event.type == BridgeEventExit) {
                running = false;
            } else if(event.type == BridgeEventCommand && !bridge->exit_requested) {
                if((event.length >= 4 && memcmp(event.command, "scan", 4) == 0) ||
                   (event.length >= 5 && memcmp(event.command, "sniff", 5) == 0) ||
                   (event.length >= 6 && memcmp(event.command, "attack", 6) == 0)) scan_active = true;
                if(event.length == 8 && memcmp(event.command, "stopscan", 8) == 0) scan_active = false;
                furi_hal_serial_tx(bridge->serial, (const uint8_t*)event.command, event.length);
                const uint8_t newline = '\n';
                furi_hal_serial_tx(bridge->serial, &newline, 1);
            }
        }
        const size_t count = furi_stream_buffer_receive(bridge->rx, bytes, sizeof(bytes), 0);
        if(count) {
            furi_mutex_acquire(bridge->rpc_mutex, FuriWaitForever);
            if(bridge->rpc) rpc_system_app_exchange_data(bridge->rpc, bytes, count);
            furi_mutex_release(bridge->rpc_mutex);
        }
        if(bridge->rx_overflow) {
            bridge->rx_overflow = false;
            static const uint8_t overflow[] = "\n[bridge: UART output overflow]\n";
            furi_mutex_acquire(bridge->rpc_mutex, FuriWaitForever);
            if(bridge->rpc) rpc_system_app_exchange_data(bridge->rpc, overflow, sizeof(overflow) - 1);
            furi_mutex_release(bridge->rpc_mutex);
        }
    }

    if(scan_active) {
        static const uint8_t stopscan[] = "stopscan\n";
        furi_hal_serial_tx(bridge->serial, stopscan, sizeof(stopscan) - 1);
    }
    furi_hal_serial_async_rx_stop(bridge->serial);
    furi_hal_serial_deinit(bridge->serial);
    furi_hal_serial_control_release(bridge->serial);
    gui_remove_view_port(bridge->gui, bridge->view_port);
    view_port_free(bridge->view_port);
    furi_record_close(RECORD_GUI);
    if(!bridge->otg_was_enabled) furi_hal_power_disable_otg();
    expansion_enable(bridge->expansion);
    furi_record_close(RECORD_EXPANSION);

    furi_mutex_acquire(bridge->rpc_mutex, FuriWaitForever);
    if(bridge->rpc) {
        rpc_system_app_set_callback(bridge->rpc, NULL, NULL);
        rpc_system_app_send_exited(bridge->rpc);
        bridge->rpc = NULL;
    }
    furi_mutex_release(bridge->rpc_mutex);
    furi_stream_buffer_free(bridge->rx);
    furi_message_queue_free(bridge->events);
    furi_mutex_free(bridge->rpc_mutex);
    free(bridge);
    return 0;
}
