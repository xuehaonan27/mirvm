#include <atomic>

namespace {
struct Marker {
    int value;
};

std::atomic<int> caught{0};
}

extern "C" void cpp_reset_caught() {
    caught.store(0, std::memory_order_relaxed);
}

extern "C" int cpp_caught_value() {
    return caught.load(std::memory_order_relaxed);
}

extern "C" int cpp_no_throw() {
    return 42;
}

extern "C" void cpp_throw_marker() {
    throw Marker{73};
}

extern "C" int cpp_call_typed_catch(void (*callback)()) {
    try {
        callback();
        return 0;
    } catch (const Marker &marker) {
        caught.store(marker.value, std::memory_order_relaxed);
        return 1000 + marker.value;
    } catch (...) {
        caught.store(999, std::memory_order_relaxed);
        return 1999;
    }
}

extern "C" void cpp_call_catch_rethrow(void (*callback)()) {
    try {
        callback();
    } catch (...) {
        caught.store(888, std::memory_order_relaxed);
        throw;
    }
}

extern "C" void cpp_call_no_catch(void (*callback)()) {
    callback();
}

extern "C" int cpp_call_catch_swallow(void (*callback)()) {
    try {
        callback();
        return 0;
    } catch (...) {
        caught.store(777, std::memory_order_relaxed);
        return 1777;
    }
}

extern "C" int cpp_call_plain_c(void (*callback)()) {
    return cpp_call_catch_swallow(callback);
}
