// In-process Rust data owner. Rust's independent control thread carries metadata only.
#import "Engine.h"
@interface OBDConnection : NSObject
@property uint64_t handle;
@property uint64_t capacity;
@property BOOL writable;
@property(strong) NSURL *directory;
- (instancetype)initWithDirectory:(NSURL *)directory writable:(BOOL)writable error:(NSError **)error;
- (NSError *)finish;
@end
@implementation OBDConnection
- (instancetype)initWithDirectory:(NSURL *)directory writable:(BOOL)writable error:(NSError **)error {
    self=[super init]; if(!self) return nil;
    NSURL *accessURL=[directory URLByAppendingPathComponent:@"device-access.json"];
    NSData *accessData=[NSData dataWithContentsOfURL:accessURL];
    if(accessData) {
        id access=[NSJSONSerialization JSONObjectWithData:accessData options:0 error:error];
        if(![access isKindOfClass:NSDictionary.class] || ![access[@"read_only"] isKindOfClass:NSNumber.class]) {
            *error=Err(EINVAL); return nil;
        }
        if([access[@"read_only"] boolValue]) writable=NO;
    }
    self.directory=directory; self.writable=writable;
    uint64_t handle=0,capacity=0;
    int code=obd_open(directory.fileSystemRepresentation,writable,&handle,&capacity);
    if(code) { *error=Err(code); return nil; }
    self.handle=handle; self.capacity=capacity; return self;
}
- (NSError *)finish {
    if(!self.handle) return nil;
    int code=obd_close(self.handle);
    if(code) return Err(code);
    self.handle=0; return nil;
}
- (void)dealloc {
    // Failed close keeps the Rust handle/lock quarantined until process exit.
    if(_handle) { int code=obd_close(_handle); if(code) NSLog(@"OBD close failed: %d",code); }
}
@end
