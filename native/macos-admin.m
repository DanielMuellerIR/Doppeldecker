// macOS verwaltet Installation und Authentifizierung. Der Helfer akzeptiert
// ausschließlich den typisierten Core-Auftrag, niemals Befehle oder Programme.
#import <Foundation/Foundation.h>
#import <Security/Security.h>
#import <ServiceManagement/ServiceManagement.h>
#include <unistd.h>
#ifndef DD_METADATA_BUILD_VERSION
#error "DD_METADATA_BUILD_VERSION muss der Cargo-Paketversion entsprechen"
#endif

static NSString *const DDServiceName = @"com.doppeldecker.desktop.metadata-helper";
static NSString *const DDPlistName = @"com.doppeldecker.desktop.metadata-helper.plist";
static NSString *const DDAppIdentifier = @"com.doppeldecker.desktop";
static const NSUInteger DDMaxRequest = 32 * 1024 * 1024;
static const NSUInteger DDMaxReply = 64 * 1024 * 1024;
typedef char *(*DDExecute)(const char *, size_t);
typedef void (*DDCancel)(const char *);
typedef void (*DDFree)(char *);
typedef bool (*DDCancelled)(void *);
static DDExecute executeRequest;
static DDCancel cancelRequest;
static DDFree freeReply;

static NSString *failure(NSString *code, NSString *message) {
    NSData *data = [NSJSONSerialization dataWithJSONObject:@{@"error":@{@"code":code, @"message":message}} options:0 error:nil];
    return [[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding];
}
static char *copyString(NSString *value) { return strdup(value.UTF8String); }
static void clearMemory(void *memory, size_t length) {
    volatile unsigned char *bytes=memory;
    while (length--) *bytes++=0;
}

// Team und Identifier stammen aus der eigenen gültigen Signatur. Ad-hoc-Builds
// erhalten keinen Ersatzwert und können den produktiven Dienst nicht benutzen.
static NSString *ownTeam(void) {
    SecCodeRef code = NULL;
    CFDictionaryRef information = NULL;
    if (SecCodeCopySelf(kSecCSDefaultFlags, &code) != errSecSuccess) return nil;
    OSStatus status = SecCodeCheckValidity(code, kSecCSStrictValidate, NULL);
    if (status == errSecSuccess) status = SecCodeCopySigningInformation(code, kSecCSSigningInformation, &information);
    CFRelease(code);
    if (status != errSecSuccess || !information) return nil;
    NSDictionary *details = CFBridgingRelease(information);
    NSString *team = details[(__bridge NSString *)kSecCodeInfoTeamIdentifier];
    if (![team isKindOfClass:NSString.class] || team.length != 10 || [team rangeOfCharacterFromSet:[[NSCharacterSet characterSetWithCharactersInString:@"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"] invertedSet]].location != NSNotFound) return nil;
    return team;
}
static NSString *requirement(NSString *identifier) {
    NSString *team = ownTeam();
    if (!team) return nil;
    return [NSString stringWithFormat:@"anchor apple generic and identifier \"%@\" and certificate leaf[subject.OU] = \"%@\"", identifier, team];
}

static bool packageReady(void) {
    NSURL *bundle=NSBundle.mainBundle.bundleURL;
    NSURL *plist=[bundle URLByAppendingPathComponent:[@"Contents/Library/LaunchDaemons" stringByAppendingPathComponent:DDPlistName]];
    NSURL *helper=[bundle URLByAppendingPathComponent:@"Contents/Library/LaunchServices/doppeldecker-metadata-helper"];
    NSDictionary *definition=[NSDictionary dictionaryWithContentsOfURL:plist];
    NSDictionary *expected=@{@"Label":DDServiceName,@"BundleProgram":@"Contents/Library/LaunchServices/doppeldecker-metadata-helper",@"MachServices":@{DDServiceName:@YES},@"RunAtLoad":@NO,@"ProcessType":@"Interactive"};
    if (![definition isEqualToDictionary:expected]) return false;
    NSNumber *regular=nil,*symlink=nil;
    [plist getResourceValue:&regular forKey:NSURLIsRegularFileKey error:nil];
    [plist getResourceValue:&symlink forKey:NSURLIsSymbolicLinkKey error:nil];
    if (!regular.boolValue || symlink.boolValue) return false;
    regular=nil; symlink=nil;
    [helper getResourceValue:&regular forKey:NSURLIsRegularFileKey error:nil];
    [helper getResourceValue:&symlink forKey:NSURLIsSymbolicLinkKey error:nil];
    if (!regular.boolValue || symlink.boolValue || ![NSFileManager.defaultManager isExecutableFileAtPath:helper.path]) return false;
    SecStaticCodeRef code=NULL; SecRequirementRef required=NULL;
    NSString *rule=requirement(DDServiceName);
    if (!rule) return false;
    OSStatus status=SecRequirementCreateWithString((__bridge CFStringRef)rule,kSecCSDefaultFlags,&required);
    if (status==errSecSuccess) status=SecStaticCodeCreateWithPath((__bridge CFURLRef)helper,kSecCSDefaultFlags,&code);
    if (status==errSecSuccess) status=SecStaticCodeCheckValidity(code,kSecCSStrictValidate,required);
    if (code) CFRelease(code); if (required) CFRelease(required);
    return status==errSecSuccess;
}

@protocol DDMetadataProtocol
- (void)versionWithReply:(void (^)(NSInteger, NSString *, NSString *))reply;
- (void)perform:(NSData *)request authorization:(NSData *)authorization challenge:(NSString *)challenge reply:(void (^)(NSString *))reply;
- (void)cancel:(NSString *)requestID;
@end

@interface DDMetadataService : NSObject <DDMetadataProtocol>
@property NSString *challenge;
@property NSString *activeID;
@property BOOL consumed;
@property BOOL invalidated;
@end
@implementation DDMetadataService
- (instancetype)init { self=[super init]; if (self) _challenge=NSUUID.UUID.UUIDString; return self; }
- (void)versionWithReply:(void (^)(NSInteger, NSString *, NSString *))reply { reply(1, self.challenge, @DD_METADATA_BUILD_VERSION); }
- (void)cancel:(NSString *)requestID {
    @synchronized(self) {
        if ([self.activeID isEqualToString:requestID]) cancelRequest(requestID.UTF8String);
    }
}
- (void)perform:(NSData *)request authorization:(NSData *)authorization challenge:(NSString *)challenge reply:(void (^)(NSString *))reply {
    @synchronized(self) {
        if (self.consumed || self.invalidated || ![challenge isEqualToString:self.challenge]) {
            reply(failure(@"invalid_input", @"Administratorauftrag bereits verwendet oder Verbindung beendet")); return;
        }
        self.consumed=YES;
    }
    if (request.length == 0 || request.length > DDMaxRequest || authorization.length != sizeof(AuthorizationExternalForm)) {
        reply(failure(@"invalid_input", @"Ungültiger Administratorauftrag")); return;
    }
    AuthorizationExternalForm external;
    memcpy(&external, authorization.bytes, sizeof(external));
    AuthorizationRef auth=NULL;
    OSStatus status=AuthorizationCreateFromExternalForm(&external, &auth);
    clearMemory(&external, sizeof(external));
    if (status == errSecSuccess) {
        AuthorizationItem item={"system.privilege.admin", 0, NULL, 0};
        AuthorizationRights rights={1, &item};
        // Kein ExtendRights und keine UI im Rootprozess: nur die gerade im
        // Client gewährte, nicht geteilte Administratorfreigabe darf genügen.
        status=AuthorizationCopyRights(auth, &rights, kAuthorizationEmptyEnvironment, kAuthorizationFlagDefaults, NULL);
    }
    if (auth) AuthorizationFree(auth, kAuthorizationFlagDefaults);
    if (status != errSecSuccess) { reply(failure(@"permission_denied", @"Administratorfreigabe fehlt oder ist abgelaufen")); return; }
    NSDictionary *value=[NSJSONSerialization JSONObjectWithData:request options:0 error:nil];
    NSString *requestID=[value isKindOfClass:NSDictionary.class] ? value[@"requestId"] : nil;
    if (![requestID isKindOfClass:NSString.class] || requestID.length == 0 || requestID.length > 128) {
        reply(failure(@"invalid_input", @"Administratorauftrags-ID fehlt")); return;
    }
    @synchronized(self) {
        self.activeID=requestID;
        if (self.invalidated) cancelRequest(requestID.UTF8String);
    }
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        @autoreleasepool {
            char *result=executeRequest(request.bytes, request.length);
            NSString *response=result ? [[NSString alloc] initWithBytes:result length:strnlen(result, DDMaxReply+1) encoding:NSUTF8StringEncoding] : nil;
            if (result) freeReply(result);
            if (!response || [response lengthOfBytesUsingEncoding:NSUTF8StringEncoding] > DDMaxReply) response=failure(@"outcome_unclear", @"Administratorergebnis nicht vollständig lesbar; Teilwirkungen prüfen");
            reply(response);
            @synchronized(self) { self.activeID=nil; }
        }
    });
}
@end

@interface DDMetadataListener : NSObject <NSXPCListenerDelegate>
@end
@implementation DDMetadataListener
- (BOOL)listener:(NSXPCListener *)listener shouldAcceptNewConnection:(NSXPCConnection *)connection {
    (void)listener;
    DDMetadataService *service=[DDMetadataService new];
    connection.exportedInterface=[NSXPCInterface interfaceWithProtocol:@protocol(DDMetadataProtocol)];
    connection.exportedObject=service;
    connection.invalidationHandler=^{
        @synchronized(service) {
            service.invalidated=YES;
            if (service.activeID) cancelRequest(service.activeID.UTF8String);
        }
    };
    [connection resume];
    return YES;
}
@end

int dd_admin_helper_run(DDExecute execute, DDCancel cancel, DDFree release) {
    @autoreleasepool {
        if (geteuid()!=0 || !execute || !cancel || !release) return 1;
        NSString *peer=requirement(DDAppIdentifier);
        if (!peer) return 1;
        executeRequest=execute; cancelRequest=cancel; freeReply=release;
        static NSXPCListener *listener;
        static DDMetadataListener *delegate;
        listener=[[NSXPCListener alloc] initWithMachServiceName:DDServiceName];
        [listener setConnectionCodeSigningRequirement:peer];
        delegate=[DDMetadataListener new]; listener.delegate=delegate;
        [listener resume]; [[NSRunLoop currentRunLoop] run];
        return 0;
    }
}

int dd_admin_service_status(void) {
    @autoreleasepool {
        if (!ownTeam()) return -1;
        int status=(int)[SMAppService daemonServiceWithPlistName:DDPlistName].status;
        if ((status==SMAppServiceStatusNotRegistered || status==SMAppServiceStatusNotFound) && !packageReady()) return -2;
        return status;
    }
}
char *dd_admin_service_change(bool enable) {
    @autoreleasepool {
        if (!ownTeam()) return copyString(failure(@"unsupported", @"Administratorhelfer benötigt das signierte App-Paket"));
        if (enable && !packageReady()) return copyString(failure(@"unsupported",@"Administratorhelfer fehlt oder seine Signatur passt nicht zum App-Paket"));
        SMAppService *service=[SMAppService daemonServiceWithPlistName:DDPlistName];
        NSError *error=nil;
        if (enable && service.status == SMAppServiceStatusEnabled) {
            // Apples asynchrones Ende bestätigt, dass der alte Prozess weg ist,
            // bevor die geänderte App ihren Helfer erneut registriert.
            dispatch_semaphore_t stopped=dispatch_semaphore_create(0);
            NSLock *guard=[NSLock new];
            __block NSError *stopError=nil;
            [service unregisterWithCompletionHandler:^(NSError *failure) {
                [guard lock]; stopError=failure; [guard unlock]; dispatch_semaphore_signal(stopped);
            }];
            if (dispatch_semaphore_wait(stopped,dispatch_time(DISPATCH_TIME_NOW,10*NSEC_PER_SEC))!=0) return copyString(failure(@"outcome_unclear",@"Das Ende des alten Administratorhelfers ist noch nicht bestätigt; Systemstatus erneut prüfen"));
            [guard lock]; error=stopError; [guard unlock];
            if (error) return copyString(failure(@"permission_denied",error.localizedDescription));
        }
        BOOL ok=enable ? [service registerAndReturnError:&error] : [service unregisterAndReturnError:&error];
        // Auch EPERM kann die erfolgreich registrierte, noch nicht erlaubte
        // Einrichtung begleiten. Der Systemstatus beschreibt diesen Zustand
        // genauer als der variierende Fehlercode der Registrierung.
        if (!ok && enable && service.status==SMAppServiceStatusRequiresApproval) return copyString(@"{\"ok\":true}");
        if (!ok) return copyString(failure(@"permission_denied", error.localizedDescription ?: @"Systemfreigabe konnte nicht geändert werden"));
        return copyString(@"{\"ok\":true}");
    }
}
void dd_admin_open_settings(void) { @autoreleasepool { [SMAppService openSystemSettingsLoginItems]; } }

char *dd_admin_client_perform(const char *bytes, size_t length, DDCancelled cancelled, void *context) {
    @autoreleasepool {
        if (!bytes || length==0 || length>DDMaxRequest || !cancelled) return copyString(failure(@"invalid_input", @"Ungültiger Administratorauftrag"));
        NSString *peer=requirement(DDServiceName);
        if (!peer) return copyString(failure(@"unsupported", @"Administratorhelfer benötigt das signierte App-Paket"));
        if (!packageReady()) return copyString(failure(@"unsupported",@"Administratorhelfer fehlt oder seine Signatur passt nicht zum App-Paket"));
        if ([SMAppService daemonServiceWithPlistName:DDPlistName].status != SMAppServiceStatusEnabled) return copyString(failure(@"permission_denied", @"Administratorhelfer zuerst einrichten und in den Systemeinstellungen erlauben"));
        NSData *request=[NSData dataWithBytes:bytes length:length];
        NSDictionary *value=[NSJSONSerialization JSONObjectWithData:request options:0 error:nil];
        NSString *requestID=[value isKindOfClass:NSDictionary.class] ? value[@"requestId"] : nil;
        if (![requestID isKindOfClass:NSString.class]) return copyString(failure(@"invalid_input", @"Administratorauftrags-ID fehlt"));
        NSXPCConnection *connection=[[NSXPCConnection alloc] initWithMachServiceName:DDServiceName options:NSXPCConnectionPrivileged];
        [connection setCodeSigningRequirement:peer];
        connection.remoteObjectInterface=[NSXPCInterface interfaceWithProtocol:@protocol(DDMetadataProtocol)];
        [connection resume];
        NSLock *guard=[NSLock new];
        dispatch_semaphore_t ready=dispatch_semaphore_create(0);
        __block NSString *challenge=nil;
        __block NSInteger protocol=0;
        __block NSString *helperBuild=nil;
        id<DDMetadataProtocol> proxy=[connection remoteObjectProxyWithErrorHandler:^(NSError *error) { (void)error; dispatch_semaphore_signal(ready); }];
        [proxy versionWithReply:^(NSInteger version, NSString *nonce, NSString *build) { [guard lock]; protocol=version; challenge=nonce; helperBuild=build; [guard unlock]; dispatch_semaphore_signal(ready); }];
        dispatch_semaphore_wait(ready, dispatch_time(DISPATCH_TIME_NOW, 5*NSEC_PER_SEC));
        [guard lock]; NSString *nonce=challenge; NSInteger version=protocol; NSString *build=helperBuild; [guard unlock];
        if (version!=1 || !nonce || ![build isEqualToString:@DD_METADATA_BUILD_VERSION]) { [connection invalidate]; return copyString(failure(@"unsupported", @"Administratorhelfer nicht erreichbar oder veraltet. Administratorzugriff aktualisieren oder neu einrichten.")); }
        if (cancelled(context)) { [connection invalidate]; return copyString(failure(@"interrupted", @"Vor Administratorfreigabe abgebrochen; unverändert")); }
        AuthorizationRef auth=NULL;
        AuthorizationExternalForm external;
        NSMutableData *authorization=nil;
        NSString *result=nil;
        @try {
            // Eine frische Referenz je Aktion; system.privilege.admin ist nicht
            // geteilt. DestroyRights verhindert die Nutzung für spätere Aktionen.
            OSStatus status=AuthorizationCreate(NULL, kAuthorizationEmptyEnvironment, kAuthorizationFlagDefaults, &auth);
            AuthorizationItem item={"system.privilege.admin", 0, NULL, 0};
            AuthorizationRights rights={1, &item};
            if (status==errSecSuccess) status=AuthorizationCopyRights(auth, &rights, kAuthorizationEmptyEnvironment, kAuthorizationFlagInteractionAllowed|kAuthorizationFlagExtendRights, NULL);
            if (status!=errSecSuccess || cancelled(context)) {
                result=failure(status==errAuthorizationCanceled || cancelled(context) ? @"interrupted" : @"permission_denied", @"Administratorfreigabe abgebrochen oder verweigert; unverändert");
            } else if (AuthorizationMakeExternalForm(auth, &external)!=errSecSuccess) {
                result=failure(@"permission_denied", @"Administratorfreigabe nicht übertragbar; unverändert");
            } else {
                authorization=[NSMutableData dataWithBytes:&external length:sizeof(external)]; clearMemory(&external, sizeof(external));
                dispatch_semaphore_t done=dispatch_semaphore_create(0);
                __block NSString *response=nil;
                id<DDMetadataProtocol> execution=[connection remoteObjectProxyWithErrorHandler:^(NSError *error) { (void)error; [guard lock]; if (!response) response=failure(@"outcome_unclear", @"Verbindung während Administratoränderung unterbrochen; Teilwirkungen vor erneutem Versuch prüfen"); [guard unlock]; dispatch_semaphore_signal(done); }];
                [execution perform:request authorization:authorization challenge:nonce reply:^(NSString *reply) { [guard lock]; if (!response) response=reply; [guard unlock]; dispatch_semaphore_signal(done); }];
                BOOL cancellationSent=NO; NSInteger waits=0;
                while (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC/10))!=0) {
                    if (!cancellationSent && cancelled(context)) { [execution cancel:requestID]; cancellationSent=YES; }
                    if (++waits>=1200) break;
                }
                [guard lock]; result=response; [guard unlock];
                if (!result) { [execution cancel:requestID]; result=failure(@"outcome_unclear", @"Administratoränderung hat nicht rechtzeitig geantwortet; Teilwirkungen prüfen"); }
            }
        } @finally {
            if (authorization) [authorization resetBytesInRange:NSMakeRange(0, authorization.length)];
            clearMemory(&external, sizeof(external));
            if (auth) AuthorizationFree(auth, kAuthorizationFlagDestroyRights);
            [connection invalidate];
        }
        return copyString(result ?: failure(@"outcome_unclear", @"Administratorergebnis fehlt; Teilwirkungen prüfen"));
    }
}
