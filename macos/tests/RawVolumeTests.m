// Direct callback tests do not claim kernel mounting / DiskImages validation.
#import "../OverlayBDFSKit/RawFileSystem.m"
#include <assert.h>
@interface RawTestVolume : OBDRawVolume
- (BOOL)dirty;
- (uint64_t)cleanSyncs;
@end
@implementation RawTestVolume
- (BOOL)dirty { return _rawDirty; }
- (uint64_t)cleanSyncs { return _rawCleanSyncs; }
@end
int main(void) { @autoreleasepool {
    char path[]="/tmp/obd-volume-test-XXXXXX";
    int fd=mkstemp(path); assert(fd>=0); assert(ftruncate(fd,4096)==0);
    RawTestVolume *v=[[RawTestVolume alloc] initWithFD:fd size:4096 writable:YES identifier:[FSVolumeIdentifier new]];
    for(FSItem *item in @[v.root,v.raw]) {
        FSItemAttributes *a=[v attrs:item];
        for(NSUInteger bit=0;bit<14;bit++) assert([a isValid:(FSItemAttribute)(1UL<<bit)]);
    }
    __block BOOL called=NO;
    [v writeContents:[@"abc" dataUsingEncoding:NSUTF8StringEncoding] toFile:v.raw atOffset:511 replyHandler:^(size_t n,NSError *e) { assert(!e && n==3); called=YES; }];
    assert(called); char buf[3]; assert(pread(fd,buf,3,511)==3 && memcmp(buf,"abc",3)==0);
    [v writeContents:[NSData dataWithBytes:"xx" length:2] toFile:v.raw atOffset:4095 replyHandler:^(size_t n,NSError *e) { assert(e.code==EINVAL && n==0); }];
    [v writeContents:[NSData dataWithBytes:"x" length:1] toFile:v.raw atOffset:-1 replyHandler:^(size_t n,NSError *e) { assert(e.code==EINVAL && n==0); }];
    struct stat st; assert(fstat(fd,&st)==0 && st.st_size==4096);
    [v synchronizeWithFlags:0 replyHandler:^(NSError *e) { assert(!e); }];
    assert(![v dirty]);
    uint64_t clean=[v cleanSyncs];
    assert(![v barrier]); assert([v cleanSyncs]==clean+1);
    int bad=open("/dev/null",O_RDWR); assert(bad>=0); v.fd=bad;
    [v writeContents:[NSData dataWithBytes:"z" length:1] toFile:v.raw atOffset:0 replyHandler:^(size_t n,NSError *e) { assert(!e && n==1); }];
    assert([v dirty]); assert([v barrier]); assert([v dirty]);
    assert([v barrier]); assert([v dirty]);
    v.fd=fd; close(bad); assert(![v barrier]); assert(![v dirty]);
    assert([v attrs:v.raw].size==4096); assert([v attrs:v.root].type==FSItemTypeDirectory);
    [v lookupItemNamed:Name(@"disk.raw") inDirectory:v.root replyHandler:^(FSItem *item,FSFileName *name,NSError *e) { assert(item==v.raw && !e); }];
    [v lookupItemNamed:Name(@"upper.data") inDirectory:v.root replyHandler:^(FSItem *item,FSFileName *name,NSError *e) { assert(!item && e.code==ENOENT); }];
    [v setAttributes:[FSItemSetAttributesRequest new] onItem:v.raw replyHandler:^(FSItemAttributes *a,NSError *e) { assert(!a && e.code==EPERM); }];
    v.writable=NO;
    [v writeContents:[NSData dataWithBytes:"x" length:1] toFile:v.raw atOffset:0 replyHandler:^(size_t n,NSError *e) { assert(e.code==EROFS && n==0); }];
    v.writable=YES; close(fd); v.fd=-1;
    [v synchronizeWithFlags:0 replyHandler:^(NSError *e) { assert(e.code==EBADF); }];
    // Engine callback diagnostics stay opt-in and are persisted only at close.
    // A closed handle exercises callback error accounting without a live image.
    char directory[]="/tmp/obd-callback-test-XXXXXX"; assert(mkdtemp(directory));
    NSURL *dir=[NSURL fileURLWithPath:@(directory) isDirectory:YES];
    NSURL *trace=[dir URLByAppendingPathComponent:@"fskit-trace.json"];
    RawTestVolume *engine=[[RawTestVolume alloc] initWithFD:-1 size:4096 writable:YES identifier:[FSVolumeIdentifier new]];
    engine.connection=[OBDConnection new];engine.connection.directory=dir;
    [engine enableEngineTrace];assert(![engine barrier]);
    [engine unmountWithReplyHandler:^{}];
    assert(![[NSFileManager defaultManager] fileExistsAtPath:trace.path]);
    assert([[NSData data] writeToURL:[dir URLByAppendingPathComponent:@"io-trace.enabled"] atomically:YES]);
    [engine enableEngineTrace];
    [engine writeContents:[NSData dataWithBytes:"x" length:1] toFile:engine.raw atOffset:0 replyHandler:^(size_t n,NSError *e) { assert(e.code==EBADF && n==0); }];
    assert(![engine barrier]);
    assert(![[NSFileManager defaultManager] fileExistsAtPath:trace.path]);
    [engine unmountWithReplyHandler:^{}];
    NSDictionary *captured=[NSJSONSerialization JSONObjectWithData:[NSData dataWithContentsOfURL:trace] options:0 error:nil];
    assert([captured[@"format"] isEqual:@"callback-wall-end-duration-v1"]);
    assert([captured[@"dropped"] unsignedLongLongValue]==0);
    NSArray *events=captured[@"events"];assert(events.count==2);
    assert([events[0][0] intValue]==2 && [events[1][0] intValue]==3);
    for(NSArray *event in events) {
        assert(event.count==6 && [event[1] unsignedLongLongValue]>0);
        assert([event[2] unsignedLongLongValue]>=[event[3] unsignedLongLongValue]);
    }
    assert([[NSFileManager defaultManager] removeItemAtURL:dir error:nil]);
    unlink(path); puts("RawVolume callbacks: bounds, fixed capacity, readonly, lookup, mutation denial, sync failure passed");
} return 0; }
