#pragma once
#import <Foundation/Foundation.h>

// The caller separately holds a lifecycle read lease for the entire callback.
// Waiting on a ticket covers earlier callbacks, including every chunk of a
// multi-call write. Later callbacks do not extend the captured boundary.
@interface OBDIOFence : NSObject {
    NSCondition *_condition;
    NSMutableArray<NSObject *> *_active;
}
- (NSObject *)enter;
- (void)waitForPreceding:(NSObject *)ticket;
- (void)leave:(NSObject *)ticket;
@end
@implementation OBDIOFence
- (instancetype)init {
    self=[super init];
    if(self) {_condition=[NSCondition new];_active=[NSMutableArray new];}
    return self;
}
- (NSObject *)enter {
    NSObject *ticket=[NSObject new];
    [_condition lock];[_active addObject:ticket];[_condition unlock];
    return ticket;
}
- (void)waitForPreceding:(NSObject *)ticket {
    [_condition lock];
    while(_active.firstObject!=ticket) [_condition wait];
    [_condition unlock];
}
- (void)leave:(NSObject *)ticket {
    [_condition lock];[_active removeObjectIdenticalTo:ticket];
    [_condition broadcast];[_condition unlock];
}
@end
