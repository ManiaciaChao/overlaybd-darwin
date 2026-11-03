// Exposes a fixed-size RAW file or a in-process OverlayBD image as disk.raw.
// No OverlayBD layer file is accepted by the production control plane.
#import "RawFileSystem.h"
#import <sys/file.h>
#import <sys/stat.h>
#import <sys/xattr.h>
#import <fcntl.h>
#import <unistd.h>
#import <time.h>
#import <pthread.h>
#import <os/log.h>
typedef struct { uint64_t calls, bytes, wait_ns, body_ns, backend_ns, errors, max_body_ns; } OBDIOStats;
static uint64_t Now(void) { return clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW); }
static NSDictionary *Stats(OBDIOStats s) {
    return @{@"calls":@(s.calls),@"bytes":@(s.bytes),@"wait_ns":@(s.wait_ns),@"body_ns":@(s.body_ns),@"backend_ns":@(s.backend_ns),@"errors":@(s.errors),@"max_body_ns":@(s.max_body_ns)};
}
static NSError *Err(int code) { return [NSError errorWithDomain:NSPOSIXErrorDomain code:code userInfo:nil]; }
static FSFileName *Name(NSString *s) { return [[FSFileName alloc] initWithString:s]; }

#import "EngineConnection.inc"
#import "IOFence.h"

@interface OBDRawVolume : FSVolume <FSVolumeOperations, FSVolumeReadWriteOperations>
{ OBDIOStats _readStats, _writeStats, _syncStats;
    uint64_t _syncFlags[8], _rawCleanSyncs, _rawFsyncNS, _rawFullsyncNS, _rawWriteEpoch, _rawSyncedEpoch;
    BOOL _rawDirty;
    pthread_rwlock_t _engineLifecycle;
    OBDIOFence *_engineFence;
    NSMutableData *_trace;
    uint64_t _traceDropped;
}
@property(strong) NSString *profileID;
@property int fd;
@property(strong) OBDConnection *connection;
@property uint64_t capacity;
@property BOOL writable;
@property(strong) FSItem *root;
@property(strong) FSItem *raw;
- (instancetype)initWithFD:(int)fd size:(uint64_t)size writable:(BOOL)writable identifier:(FSVolumeIdentifier *)identifier;
- (NSError *)barrier;
- (NSError *)finishConnection;
- (void)enableEngineTrace;
- (void)trace:(uint64_t)kind entered:(uint64_t)entered backend:(uint64_t)backend bytes:(uint64_t)bytes flags:(uint64_t)flags;
- (NSError *)barrierWithFlags:(NSUInteger)flags;
@end
@implementation OBDRawVolume
- (instancetype)initWithFD:(int)fd size:(uint64_t)size writable:(BOOL)writable identifier:(FSVolumeIdentifier *)identifier {
    self = [super initWithVolumeID:identifier volumeName:Name(@"OverlayBD RAW PoC")];
    if (self) { pthread_rwlock_init(&_engineLifecycle,NULL); _engineFence=[OBDIOFence new]; _fd=fd; _capacity=size; _writable=writable; _root=[FSItem new]; _raw=[FSItem new]; _profileID=NSUUID.UUID.UUIDString; _rawDirty=YES; }
    char enabled=0;
    if(self && fd>=0 && fgetxattr(fd,"dev.overlaybd.raw-trace",&enabled,1,0,0)==1 && enabled=='1') _trace=[NSMutableData data];
    return self;
}
- (void)trace:(uint64_t)kind entered:(uint64_t)entered backend:(uint64_t)backend bytes:(uint64_t)bytes flags:(uint64_t)flags {
    if(!_trace) return;
    // Bounded in-memory diagnostics; emit only after timed I/O at unmount.
    if(_trace.length>=65536*6*sizeof(uint64_t)) { _traceDropped++; return; }
    uint64_t event[]={kind,clock_gettime_nsec_np(CLOCK_REALTIME),Now()-entered,backend,bytes,flags};
    [_trace appendBytes:event length:sizeof(event)];
}
- (void)enableEngineTrace {
    NSURL *flag=[self.connection.directory URLByAppendingPathComponent:@"io-trace.enabled"];
    if([[NSFileManager defaultManager] fileExistsAtPath:flag.path]) _trace=[NSMutableData data];
}
- (void)dealloc { if (_fd >= 0) close(_fd); pthread_rwlock_destroy(&_engineLifecycle); }
- (NSInteger)maximumLinkCount { return 1; }
- (NSInteger)maximumNameLength { return 255; }
- (BOOL)restrictsOwnershipChanges { return YES; }
- (BOOL)truncatesLongNames { return NO; }
- (uint64_t)maximumFileSize { return self.capacity; }
- (NSInteger)maximumXattrSize { return 0; }
- (FSVolumeSupportedCapabilities *)supportedVolumeCapabilities {
    FSVolumeSupportedCapabilities *c=[FSVolumeSupportedCapabilities new];
    c.caseFormat=FSVolumeCaseFormatSensitive; c.doesNotSupportSettingFilePermissions=YES;
    c.doesNotSupportRootTimes=YES; return c;
}
- (FSStatFSResult *)volumeStatistics {
    FSStatFSResult *s=[[FSStatFSResult alloc] initWithFileSystemTypeName:@"obdraw"];
    s.blockSize=512; s.ioSize=1024*1024; s.totalBytes=self.capacity; s.usedBytes=self.capacity;
    s.totalFiles=2; s.freeFiles=0; s.freeBytes=0; s.availableBytes=0; return s;
}
- (FSItemAttributes *)attrs:(FSItem *)item {
    FSItemAttributes *a=[FSItemAttributes new]; BOOL root=item==self.root;
    a.type=root ? FSItemTypeDirectory : FSItemTypeFile; a.mode=root ? 0500 : (self.writable ? 0600 : 0400);
    a.uid=getuid(); a.gid=getgid(); a.linkCount=1; a.size=root ? 0 : self.capacity;
    a.allocSize=a.size; a.fileID=root ? FSItemIDRootDirectory : (FSItemID)3;
    a.parentID=FSItemIDRootDirectory; a.inhibitKernelOffloadedIO=YES;
    // Synthetic proxy metadata is stable; the guest filesystem owns guest times.
    // FSKit requires these fields even when callers do not request them explicitly.
    a.flags=0; a.supportsLimitedXAttrs=NO;
    struct timespec epoch={0,0};
    a.accessTime=epoch; a.modifyTime=epoch; a.changeTime=epoch;
    a.birthTime=epoch; a.backupTime=epoch; a.addedTime=epoch;
    return a;
}
- (void)activateWithOptions:(FSTaskOptions *)options replyHandler:(void (^)(FSItem *, NSError *))reply {
    pthread_rwlock_wrlock(&_engineLifecycle);
    NSError *error=nil;
    @synchronized(self) {
        if(self.connection && !self.connection.handle) {
            OBDConnection *next=[[OBDConnection alloc] initWithDirectory:self.connection.directory writable:self.writable error:&error];
            if(next) {self.connection=next;self.capacity=next.capacity;self.writable=next.writable;}
        }
    }
    pthread_rwlock_unlock(&_engineLifecycle);reply(error ? nil : self.root,error);
}
- (void)deactivateWithOptions:(FSDeactivateOptions)options replyHandler:(void (^)(NSError *))reply { reply([self barrier]); }
- (void)mountWithOptions:(FSTaskOptions *)options replyHandler:(void (^)(NSError *))reply { reply(nil); }
- (NSError *)finishConnection {
    pthread_rwlock_wrlock(&_engineLifecycle);
    NSError *error=nil;
    @synchronized(self) { if(self.connection) error=[self.connection finish]; }
    pthread_rwlock_unlock(&_engineLifecycle);
    return error;
}
- (void)unmountWithReplyHandler:(void (^)(void))reply {
    NSError *connectionError=self.connection ? [self finishConnection] : nil;
    @synchronized(self) {
        // FSKit has stopped this volume's I/O. Close performs a local barrier
        // and writes a durable close receipt. On failure keep the lease;
        // this callback has no error parameter, so control must check leases.
        NSError *error=self.connection ? connectionError : [self barrier];
        if(error) NSLog(@"OBD unmount retained backend state: %@",error);
        NSDictionary *profile=@{@"id":self.profileID, @"backend":self.connection ? @"overlaybd" : @"raw", @"read":Stats(_readStats), @"write":Stats(_writeStats), @"sync":Stats(_syncStats), @"raw_sync":@{@"clean_calls":@(_rawCleanSyncs), @"fsync_ns":@(_rawFsyncNS), @"fullsync_ns":@(_rawFullsyncNS), @"write_epoch":@(_rawWriteEpoch), @"synced_epoch":@(_rawSyncedEpoch), @"flags":@[@(_syncFlags[0]),@(_syncFlags[1]),@(_syncFlags[2]),@(_syncFlags[3]),@(_syncFlags[4]),@(_syncFlags[5]),@(_syncFlags[6]),@(_syncFlags[7])]}};
        NSData *json=[NSJSONSerialization dataWithJSONObject:profile options:0 error:nil];
        NSString *text=[[NSString alloc] initWithData:json encoding:NSUTF8StringEncoding];
        // Aggregate counters only, emitted once at unmount; no paths or I/O data.
        os_log(OS_LOG_DEFAULT,"OBD_PROFILE %{public}s",text.UTF8String);
        if(_trace && self.connection) {
            // Diagnostic-only, bounded callback timing. Persist after backend
            // close, never on the measured I/O path. The benchmark requires
            // this file when tracing and rejects missing/truncated diagnostics.
            const uint64_t *events=_trace.bytes; NSUInteger count=_trace.length/(6*sizeof(uint64_t));
            NSMutableArray *rows=[NSMutableArray arrayWithCapacity:count];
            for(NSUInteger i=0;i<count;i++) {
                const uint64_t *v=events+6*i;
                [rows addObject:@[@(v[0]),@(v[1]),@(v[2]),@(v[3]),@(v[4]),@(v[5])]];
            }
            NSError *traceError=nil;
            NSData *data=[NSJSONSerialization dataWithJSONObject:@{@"format":@"callback-wall-end-duration-v1",@"dropped":@(_traceDropped),@"events":rows} options:0 error:&traceError];
            NSURL *destination=[self.connection.directory URLByAppendingPathComponent:@"fskit-trace.json"];
            if(!data || ![data writeToURL:destination options:NSDataWritingAtomic error:&traceError])
                NSLog(@"OBD callback trace failed: %@",traceError);
        } else if(_trace) {
            const uint64_t *events=_trace.bytes; NSUInteger count=_trace.length/(6*sizeof(uint64_t));
            for(NSUInteger i=0;i<count;i+=8) {
                NSMutableArray *batch=[NSMutableArray array];
                for(NSUInteger j=i;j<MIN(i+8,count);j++) {
                    const uint64_t *v=events+6*j;
                    [batch addObject:@[@(v[0]),@(v[1]),@(v[2]),@(v[3]),@(v[4]),@(v[5])]];
                }
                NSData *data=[NSJSONSerialization dataWithJSONObject:@{@"id":self.profileID,@"base":@(i),@"total":@(count),@"dropped":@(_traceDropped),@"events":batch} options:0 error:nil];
                NSString *line=[[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding];
                os_log(OS_LOG_DEFAULT,"OBD_TRACE %{public}s",line.UTF8String);
            }
        }
        reply();
    }
}
- (NSError *)barrier { return [self barrierWithFlags:NSUIntegerMax]; }
- (NSError *)barrierWithFlags:(NSUInteger)flags {
    uint64_t entered=Now();
    if(self.connection) {
        pthread_rwlock_rdlock(&_engineLifecycle);
        NSObject *ticket=[_engineFence enter];
        [_engineFence waitForPreceding:ticket];
        uint64_t started=Now(), handle=0;
        @synchronized(self) {handle=self.connection.handle;}
        int code=handle ? obd_flush(handle) : 0;
        uint64_t elapsed=Now()-started;
        @synchronized(self) {
            if(flags!=NSUIntegerMax) _syncFlags[flags<7 ? flags : 7]++;
            _syncStats.calls++;_syncStats.wait_ns+=started-entered;_syncStats.body_ns+=elapsed;_syncStats.backend_ns+=elapsed;
            _syncStats.max_body_ns=MAX(_syncStats.max_body_ns,elapsed);if(code) _syncStats.errors++;
            if(_trace) [self trace:3 entered:entered backend:elapsed bytes:0 flags:flags];
        }
        [_engineFence leave:ticket];pthread_rwlock_unlock(&_engineLifecycle);return code ? Err(code) : nil;
    }
    @synchronized(self) {
        if(flags!=NSUIntegerMax) _syncFlags[flags<7 ? flags : 7]++;
        uint64_t started=Now(); _syncStats.calls++; _syncStats.wait_ns+=started-entered;
        NSError *error=nil;
        uint64_t backend=Now();
        if(self.connection) {
            if(self.connection.handle) { int code=obd_flush(self.connection.handle); if(code) error=Err(code); }
        } else if(self.writable) {
            if(self.fd<0) error=Err(EBADF);
            else if(!_rawDirty) _rawCleanSyncs++;
            else {
                // F_FULLFSYNC includes file synchronization. Do not precede it
                // with a second fsync. Keep all flags conservative until their
                // end-to-end DiskImages durability semantics are established.
                uint64_t phase=Now();
                int rc=fcntl(self.fd,F_FULLFSYNC), savedErrno=errno;
                _rawFullsyncNS+=Now()-phase;
                if(rc) error=Err(savedErrno);
                else { _rawDirty=NO; _rawSyncedEpoch=_rawWriteEpoch; }
            }
        }
        uint64_t backendElapsed=Now()-backend;
        _syncStats.backend_ns+=backendElapsed;
        uint64_t elapsed=Now()-started;
        _syncStats.body_ns+=elapsed; _syncStats.max_body_ns=MAX(_syncStats.max_body_ns,elapsed);
        if(error) _syncStats.errors++;
        [self trace:3 entered:entered backend:backendElapsed bytes:0 flags:flags];
        return error;
    }
}
- (void)synchronizeWithFlags:(FSSyncFlags)flags replyHandler:(void (^)(NSError *))reply { reply([self barrierWithFlags:(NSUInteger)flags]); }
- (void)reclaimItem:(FSItem *)item replyHandler:(void (^)(NSError *))reply { reply(nil); }
- (void)lookupItemNamed:(FSFileName *)name inDirectory:(FSItem *)directory replyHandler:(void (^)(FSItem *, FSFileName *, NSError *))reply {
    if(directory!=self.root) { reply(nil,nil,Err(ENOTDIR)); return; }
    if([name.string isEqual:@"disk.raw"]) reply(self.raw,Name(@"disk.raw"),nil);
    else if([name.string isEqual:@"."] || [name.string isEqual:@".."]) reply(self.root,name,nil);
    else reply(nil,nil,Err(ENOENT));
}
- (void)getAttributes:(FSItemGetAttributesRequest *)request ofItem:(FSItem *)item replyHandler:(void (^)(FSItemAttributes *, NSError *))reply {
    if(item!=self.root && item!=self.raw) reply(nil,Err(ESTALE)); else reply([self attrs:item],nil);
}
- (void)setAttributes:(FSItemSetAttributesRequest *)request onItem:(FSItem *)item replyHandler:(void (^)(FSItemAttributes *, NSError *))reply { reply(nil,Err(EPERM)); }
- (void)enumerateDirectory:(FSItem *)directory startingAtCookie:(FSDirectoryCookie)cookie verifier:(FSDirectoryVerifier)verifier providingAttributes:(FSItemGetAttributesRequest *)attributes usingPacker:(FSDirectoryEntryPacker *)packer replyHandler:(void (^)(FSDirectoryVerifier,NSError *))reply {
    if(directory!=self.root) { reply(0,Err(ENOTDIR)); return; }
    if(cookie>3 || (verifier!=0 && verifier!=1)) { reply(0,Err(EINVAL)); return; }
    NSArray *names=@[@".",@"..",@"disk.raw"];
    for(NSUInteger i=cookie;i<3;i++) {
        if(attributes && i<2) continue;
        FSItem *item=i==2 ? self.raw : self.root;
        if(![packer packEntryWithName:Name(names[i]) itemType:i==2 ? FSItemTypeFile : FSItemTypeDirectory itemID:i==2 ? (FSItemID)3 : FSItemIDRootDirectory nextCookie:i+1 attributes:attributes ? [self attrs:item] : nil]) break;
    }
    reply(1,nil);
}
- (void)readFromFile:(FSItem *)item offset:(off_t)offset length:(size_t)length intoBuffer:(FSMutableFileDataBuffer *)buffer replyHandler:(void (^)(size_t,NSError *))reply {
    uint64_t entered=Now();
    if(self.connection) {
        pthread_rwlock_rdlock(&_engineLifecycle);
        NSObject *ticket=[_engineFence enter];
        uint64_t started=Now(), handle=0, capacity=0;
        @synchronized(self) {handle=self.connection.handle;capacity=self.capacity;}
        int code=0;size_t done=0;
        if(item!=self.raw || offset<0 || length>buffer.length) code=EINVAL;
        else if(!handle) code=EBADF;
        else {
            size_t wanted=(uint64_t)offset>=capacity ? 0 : MIN(length,capacity-(uint64_t)offset);
            while(done<wanted) {size_t n=MIN(wanted-done,4*1024*1024),actual=0;
                code=obd_read(handle,offset+done,(char *)buffer.mutableBytes+done,n,&actual);done+=actual;
                if(code || actual!=n) {code=code ?: EIO;break;}
            }
        }
        uint64_t elapsed=Now()-started;
        @synchronized(self) {_readStats.calls++;_readStats.bytes+=done;_readStats.wait_ns+=started-entered;
            _readStats.body_ns+=elapsed;_readStats.backend_ns+=elapsed;_readStats.max_body_ns=MAX(_readStats.max_body_ns,elapsed);if(code) _readStats.errors++;
            if(_trace) [self trace:1 entered:entered backend:elapsed bytes:done flags:0];}
        [_engineFence leave:ticket];pthread_rwlock_unlock(&_engineLifecycle);reply(done,code ? Err(code) : nil);return;
    }
    @synchronized(self) {
        uint64_t started=Now(); _readStats.calls++; _readStats.wait_ns+=started-entered;
        uint64_t backendBefore=_readStats.backend_ns;
        void (^finish)(size_t,NSError *)=^(size_t actual,NSError *error) {
            [self trace:1 entered:entered backend:self->_readStats.backend_ns-backendBefore bytes:actual flags:0];
            uint64_t elapsed=Now()-started;
            self->_readStats.bytes+=actual; self->_readStats.body_ns+=elapsed;
            self->_readStats.max_body_ns=MAX(self->_readStats.max_body_ns,elapsed);
            if(error) self->_readStats.errors++;
            reply(actual,error);
        };
        if(item!=self.raw || offset<0 || length>buffer.length) { finish(0,Err(EINVAL)); return; }
        if((uint64_t)offset>=self.capacity) { finish(0,nil); return; }
        size_t wanted=MIN(length,self.capacity-(uint64_t)offset), done=0;
        while(done<wanted) {
            if(self.connection) {
                size_t chunk=MIN(wanted-done,4*1024*1024), actual=0;
                uint64_t backend=Now();
                int code=obd_read(self.connection.handle,offset+done,(char *)buffer.mutableBytes+done,chunk,&actual);
                _readStats.backend_ns+=Now()-backend;
                done+=actual;
                if(code || actual!=chunk) { finish(done,Err(code ?: EIO)); return; }
                continue;
            }
            uint64_t backend=Now();
            ssize_t n=pread(self.fd,(char *)buffer.mutableBytes+done,wanted-done,offset+done);
            int savedErrno=errno; _readStats.backend_ns+=Now()-backend;
            if(n<0 && savedErrno==EINTR) continue;
            if(n<=0) { finish(done,Err(n<0 ? savedErrno : EIO)); return; } done+=(size_t)n;
        }
        finish(done,nil);
    }
}
- (void)writeContents:(NSData *)contents toFile:(FSItem *)item atOffset:(off_t)offset replyHandler:(void (^)(size_t,NSError *))reply {
    uint64_t entered=Now();
    if(self.connection) {
        pthread_rwlock_rdlock(&_engineLifecycle);
        NSObject *ticket=[_engineFence enter];
        uint64_t started=Now(), handle=0, capacity=0;BOOL writable=NO;
        @synchronized(self) {handle=self.connection.handle;capacity=self.capacity;writable=self.writable;}
        int code=0;size_t done=0;
        if(!writable) code=EROFS;
        else if(item!=self.raw || offset<0 || (uint64_t)offset>capacity || contents.length>capacity-(uint64_t)offset) code=EINVAL;
        else if(!handle) code=EBADF;
        else {while(done<contents.length) {size_t n=MIN(contents.length-done,4*1024*1024),actual=0;
            code=obd_write(handle,offset+done,(const char *)contents.bytes+done,n,&actual);done+=actual;
            if(code || actual!=n) {code=code ?: EIO;break;}
        }}
        uint64_t elapsed=Now()-started;
        @synchronized(self) {_writeStats.calls++;_writeStats.bytes+=done;_writeStats.wait_ns+=started-entered;
            _writeStats.body_ns+=elapsed;_writeStats.backend_ns+=elapsed;_writeStats.max_body_ns=MAX(_writeStats.max_body_ns,elapsed);if(code) _writeStats.errors++;
            if(_trace) [self trace:2 entered:entered backend:elapsed bytes:done flags:0];}
        [_engineFence leave:ticket];pthread_rwlock_unlock(&_engineLifecycle);reply(done,code ? Err(code) : nil);return;
    }
    @synchronized(self) {
        uint64_t started=Now(); _writeStats.calls++; _writeStats.wait_ns+=started-entered;
        uint64_t backendBefore=_writeStats.backend_ns;
        void (^finish)(size_t,NSError *)=^(size_t actual,NSError *error) {
            [self trace:2 entered:entered backend:self->_writeStats.backend_ns-backendBefore bytes:actual flags:0];
            uint64_t elapsed=Now()-started;
            self->_writeStats.bytes+=actual; self->_writeStats.body_ns+=elapsed;
            self->_writeStats.max_body_ns=MAX(self->_writeStats.max_body_ns,elapsed);
            if(error) self->_writeStats.errors++;
            reply(actual,error);
        };
        if(!self.writable) { finish(0,Err(EROFS)); return; }
        if(item!=self.raw || offset<0 || (uint64_t)offset>self.capacity || contents.length>self.capacity-(uint64_t)offset) { finish(0,Err(EINVAL)); return; }
        size_t done=0;
        while(done<contents.length) {
            if(self.connection) {
                size_t chunk=MIN(contents.length-done,4*1024*1024), actual=0;
                uint64_t backend=Now();
                int code=obd_write(self.connection.handle,offset+done,(const char *)contents.bytes+done,chunk,&actual);
                _writeStats.backend_ns+=Now()-backend;
                done+=actual;
                if(code || actual!=chunk) { finish(done,Err(code ?: EIO)); return; }
                continue;
            }
            uint64_t backend=Now();
            // Include partial/failed writes in the next barrier. The monitor
            // serializes this generation with synchronization and close.
            _rawDirty=YES; _rawWriteEpoch++;
            ssize_t n=pwrite(self.fd,(const char *)contents.bytes+done,contents.length-done,offset+done);
            int savedErrno=errno; _writeStats.backend_ns+=Now()-backend;
            if(n<0 && savedErrno==EINTR) continue;
            if(n<=0) { finish(done,Err(n<0 ? savedErrno : EIO)); return; } done+=(size_t)n;
        }
        finish(done,nil);
    }
}
- (void)createItemNamed:(FSFileName *)name type:(FSItemType)type inDirectory:(FSItem *)directory attributes:(FSItemSetAttributesRequest *)attributes replyHandler:(void (^)(FSItem *,FSFileName *,NSError *))reply { reply(nil,nil,Err(EPERM)); }
- (void)createSymbolicLinkNamed:(FSFileName *)name inDirectory:(FSItem *)directory attributes:(FSItemSetAttributesRequest *)attributes linkContents:(FSFileName *)contents replyHandler:(void (^)(FSItem *,FSFileName *,NSError *))reply { reply(nil,nil,Err(EPERM)); }
- (void)createLinkToItem:(FSItem *)item named:(FSFileName *)name inDirectory:(FSItem *)directory replyHandler:(void (^)(FSFileName *,NSError *))reply { reply(nil,Err(EPERM)); }
- (void)renameItem:(FSItem *)item inDirectory:(FSItem *)sourceDirectory named:(FSFileName *)sourceName toNewName:(FSFileName *)destinationName inDirectory:(FSItem *)destinationDirectory overItem:(FSItem *)overItem replyHandler:(void (^)(FSFileName *,NSError *))reply { reply(nil,Err(EPERM)); }
- (void)removeItem:(FSItem *)item named:(FSFileName *)name fromDirectory:(FSItem *)directory replyHandler:(void (^)(NSError *))reply { reply(Err(EPERM)); }
- (void)readSymbolicLink:(FSItem *)item replyHandler:(void (^)(FSFileName *,NSError *))reply { reply(nil,Err(EINVAL)); }
@end

@interface OBDRawFileSystem ()
@property(strong) FSPathURLResource *resource;
@property(strong) OBDRawVolume *volume;
@property(strong) FSContainerIdentifier *identity;
@end
@implementation OBDRawFileSystem
- (instancetype)init { self=[super init]; if(self) _identity=[[FSContainerIdentifier alloc] init]; return self; }
- (void)probeResource:(FSResource *)resource replyHandler:(void (^)(FSProbeResult *,NSError *))reply {
    if(![resource isKindOfClass:FSPathURLResource.class]) { reply(FSProbeResult.notRecognizedProbeResult,nil); return; }
    reply([FSProbeResult usableProbeResultWithName:@"OverlayBD RAW PoC" containerID:self.identity],nil);
}
- (void)loadResource:(FSResource *)resource options:(FSTaskOptions *)options replyHandler:(void (^)(FSVolume *,NSError *))reply {
    @synchronized(self) {
        if(self.resource) { reply(nil,Err(EBUSY)); return; }
        if(![resource isKindOfClass:FSPathURLResource.class]) { reply(nil,Err(EINVAL)); return; }
        FSPathURLResource *path=(FSPathURLResource *)resource;
        if(![path.url startAccessingSecurityScopedResource]) { reply(nil,Err(EACCES)); return; }
        // Path URL resources supplied by mount(8) report isWritable=NO even
        // for -w mounts on macOS 26. Access is authorized by the scoped URL
        // and open(2), as in Apple's Passthrough sample. A read-only security
        // scope may reject O_RDWR; then expose explicitly read-only attributes.
        BOOL writable=![options.taskOptions containsObject:@"--rdonly"];
        struct stat resourceStat;
        if(lstat(path.url.fileSystemRepresentation,&resourceStat)) {
            int code=errno; [path.url stopAccessingSecurityScopedResource]; reply(nil,Err(code)); return;
        }
        if(S_ISDIR(resourceStat.st_mode)) {
            NSError *error=nil;
            OBDConnection *connection=[[OBDConnection alloc] initWithDirectory:path.url writable:writable error:&error];
            if(!connection) { [path.url stopAccessingSecurityScopedResource]; reply(nil,error ?: Err(EIO)); return; }
            self.resource=path;
            self.volume=[[OBDRawVolume alloc] initWithFD:-1 size:connection.capacity writable:connection.writable identifier:self.identity.volumeIdentifier];
            self.volume.connection=connection;
            [self.volume enableEngineTrace];
            self.containerStatus=FSContainerStatus.ready; reply(self.volume,nil); return;
        }
        int fd=open(path.url.fileSystemRepresentation,(writable ? O_RDWR : O_RDONLY)|O_NOFOLLOW|O_CLOEXEC);
        if(fd<0 && writable && (errno==EACCES || errno==EPERM || errno==EROFS)) {
            writable=NO;
            fd=open(path.url.fileSystemRepresentation,O_RDONLY|O_NOFOLLOW|O_CLOEXEC);
        }
        struct stat st; int code=0;
        if(fd<0) code=errno;
        else if(fstat(fd,&st)) code=errno;
        else if(!S_ISREG(st.st_mode) || st.st_size<=0 || st.st_size%512) code=EINVAL;
        else if(flock(fd,LOCK_EX|LOCK_NB)) code=errno;
        if(code) { if(fd>=0) close(fd); [path.url stopAccessingSecurityScopedResource]; reply(nil,Err(code)); return; }
        self.resource=path;
        self.volume=[[OBDRawVolume alloc] initWithFD:fd size:st.st_size writable:writable identifier:self.identity.volumeIdentifier];
        self.containerStatus=FSContainerStatus.ready; reply(self.volume,nil);
    }
}
- (void)unloadResource:(FSResource *)resource options:(FSTaskOptions *)options replyHandler:(void (^)(NSError *))reply {
    @synchronized(self) {
        if(![resource isKindOfClass:FSPathURLResource.class] || ![[(FSPathURLResource *)resource url] isEqual:self.resource.url]) { reply(Err(EINVAL)); return; }
        NSError *error=[self.volume barrier]; if(error) { reply(error); return; }
        if(self.volume.connection) { error=[self.volume finishConnection]; if(error) { reply(error); return; } }
        self.volume=nil; [self.resource.url stopAccessingSecurityScopedResource]; self.resource=nil; reply(nil);
    }
}
@end
