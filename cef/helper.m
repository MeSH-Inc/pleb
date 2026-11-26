// pleb Helper (macOS): CEF subprocess entry point for OSR defaults.

#import <Cocoa/Cocoa.h>

#include "helper_core.h"

int main(int argc, char* argv[]) {
    @autoreleasepool {
        return pleb_run_helper_process(argc, argv);
    }
}
