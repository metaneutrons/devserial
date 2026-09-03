// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

#import <Cocoa/Cocoa.h>
#import <objc/runtime.h>

static NSString *g_version = @"0.1.4";
static NSImage *g_icon = nil;

@implementation NSMenuItem (DevSerialMenuFix)

+ (void)load {
    static dispatch_once_t onceToken;
    dispatch_once(&onceToken, ^{
        Class class = [self class];
        SEL origSel = @selector(setTitle:);
        SEL swizSel = @selector(devserial_setTitle:);

        Method origMethod = class_getInstanceMethod(class, origSel);
        Method swizMethod = class_getInstanceMethod(class, swizSel);

        if (origMethod && swizMethod) {
            method_exchangeImplementations(origMethod, swizMethod);
        }
    });
}

- (void)devserial_setTitle:(NSString *)title {
    if ([NSApp mainMenu] && [self menu] == [NSApp mainMenu] && [[NSApp mainMenu] indexOfItem:self] == 0) {
        [self devserial_setTitle:@"devserial"];
        return;
    }
    [self devserial_setTitle:title];
}

@end

@implementation NSMenu (DevSerialMenuFix)

+ (void)load {
    static dispatch_once_t onceToken;
    dispatch_once(&onceToken, ^{
        Class class = [self class];
        SEL origSel = @selector(setTitle:);
        SEL swizSel = @selector(devserial_setMenuTitle:);

        Method origMethod = class_getInstanceMethod(class, origSel);
        Method swizMethod = class_getInstanceMethod(class, swizSel);

        if (origMethod && swizMethod) {
            method_exchangeImplementations(origMethod, swizMethod);
        }
    });
}

- (void)devserial_setMenuTitle:(NSString *)title {
    if ([NSApp mainMenu] && [[NSApp mainMenu] numberOfItems] > 0 && self == [[[NSApp mainMenu] itemAtIndex:0] submenu]) {
        [self devserial_setMenuTitle:@"devserial"];
        return;
    }
    [self devserial_setMenuTitle:title];
}

@end

typedef struct {
    BOOL is_text_focused;
    BOOL has_text_selection;
    BOOL text_field_not_empty;
    BOOL buffer_not_empty;
    BOOL has_buffer_selection;
    BOOL can_undo;
    BOOL can_redo;
} DevSerialEditState;

static DevSerialEditState g_editState = {
    .is_text_focused = NO,
    .has_text_selection = NO,
    .text_field_not_empty = NO,
    .buffer_not_empty = NO,
    .has_buffer_selection = NO,
    .can_undo = NO,
    .can_redo = NO,
};

void devserial_update_edit_state(
    BOOL is_text_focused,
    BOOL has_text_selection,
    BOOL text_field_not_empty,
    BOOL buffer_not_empty,
    BOOL has_buffer_selection,
    BOOL can_undo,
    BOOL can_redo
) {
    g_editState.is_text_focused = is_text_focused;
    g_editState.has_text_selection = has_text_selection;
    g_editState.text_field_not_empty = text_field_not_empty;
    g_editState.buffer_not_empty = buffer_not_empty;
    g_editState.has_buffer_selection = has_buffer_selection;
    g_editState.can_undo = can_undo;
    g_editState.can_redo = can_redo;
}

@interface NSView (DevSerialEditSupport) <NSMenuItemValidation>
@end

@implementation NSView (DevSerialEditSupport)

- (BOOL)validateMenuItem:(NSMenuItem *)menuItem {
    SEL action = [menuItem action];

    // Undo / Redo: Only allowed in editable text fields when content exists / undoable
    if (action == @selector(undo:)) {
        return g_editState.is_text_focused && (g_editState.can_undo || g_editState.text_field_not_empty);
    }
    if (action == @selector(redo:)) {
        return g_editState.is_text_focused && g_editState.can_redo;
    }

    // Cut (Cmd+X):
    // In editable text fields: enabled ONLY if text is selected.
    // In read-only buffer: ALWAYS DISABLED.
    if (action == @selector(cut:)) {
        return g_editState.is_text_focused && g_editState.has_text_selection;
    }

    // Copy (Cmd+C):
    // In editable text fields: enabled ONLY if text is selected.
    // In read-only buffer: enabled if text in buffer is selected OR if buffer is not empty.
    if (action == @selector(copy:)) {
        if (g_editState.is_text_focused) {
            return g_editState.has_text_selection;
        }
        return g_editState.has_buffer_selection || g_editState.buffer_not_empty;
    }

    // Paste (Cmd+V):
    // In editable text fields: enabled ONLY if clipboard has text.
    // In read-only buffer: ALWAYS DISABLED (cannot paste into buffer!).
    if (action == @selector(paste:)) {
        if (!g_editState.is_text_focused) {
            return NO;
        }
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        NSString *type = [pb availableTypeFromArray:@[NSPasteboardTypeString]];
        return (type != nil);
    }

    // Select All (Cmd+A):
    // In editable text fields: enabled if field is not empty.
    // In read-only buffer: enabled if buffer has lines.
    if (action == @selector(selectAll:)) {
        if (g_editState.is_text_focused) {
            return g_editState.text_field_not_empty;
        }
        return g_editState.buffer_not_empty;
    }

    return YES;
}

static void forward_edit_action(id target, unsigned short keyCode, NSString *chars, NSEventModifierFlags extraFlags) {
    NSEvent *event = [NSApp currentEvent];
    if (event && [event type] == NSEventTypeKeyDown) {
        if ([target respondsToSelector:@selector(keyDown:)]) {
            [target keyDown:event];
            return;
        }
    }

    NSWindow *keyWindow = [NSApp keyWindow];
    if (!keyWindow) return;

    NSEventModifierFlags flags = NSEventModifierFlagCommand | extraFlags;
    NSTimeInterval now = [[NSProcessInfo processInfo] systemUptime];
    NSInteger winNum = [keyWindow windowNumber];

    NSEvent *keyDown = [NSEvent keyEventWithType:NSEventTypeKeyDown
                                        location:NSZeroPoint
                                   modifierFlags:flags
                                       timestamp:now
                                    windowNumber:winNum
                                         context:nil
                                      characters:chars
                     charactersIgnoringModifiers:chars
                                       isARepeat:NO
                                         keyCode:keyCode];

    NSEvent *keyUp = [NSEvent keyEventWithType:NSEventTypeKeyUp
                                      location:NSZeroPoint
                                 modifierFlags:flags
                                     timestamp:now
                                  windowNumber:winNum
                                       context:nil
                                    characters:chars
                   charactersIgnoringModifiers:chars
                                     isARepeat:NO
                                       keyCode:keyCode];

    if ([target respondsToSelector:@selector(keyDown:)]) {
        [target keyDown:keyDown];
        if ([target respondsToSelector:@selector(keyUp:)]) {
            [target keyUp:keyUp];
        }
    }
}

- (void)undo:(id)sender { (void)sender; forward_edit_action(self, 6, @"z", 0); }
- (void)redo:(id)sender { (void)sender; forward_edit_action(self, 6, @"Z", NSEventModifierFlagShift); }
- (void)cut:(id)sender { (void)sender; forward_edit_action(self, 7, @"x", 0); }
- (void)copy:(id)sender { (void)sender; forward_edit_action(self, 8, @"c", 0); }
- (void)paste:(id)sender { (void)sender; forward_edit_action(self, 9, @"v", 0); }
- (void)selectAll:(id)sender { (void)sender; forward_edit_action(self, 0, @"a", 0); }

@end

@interface DevSerialAboutHandler : NSObject
- (void)showAbout:(id)sender;
- (void)openHelp:(id)sender;
- (void)exportBuffer:(id)sender;
- (void)openPort:(id)sender;
- (void)showPortSettings:(id)sender;
- (void)toggleConnect:(id)sender;
@end

static BOOL g_exportRequested = NO;
static BOOL g_openPortRequested = NO;
static BOOL g_portSettingsRequested = NO;
static BOOL g_toggleConnectRequested = NO;

@implementation DevSerialAboutHandler
- (void)showAbout:(id)sender {
    (void)sender;
    NSMutableDictionary *options = [NSMutableDictionary dictionary];
    options[NSAboutPanelOptionApplicationName] = @"devserial";
    options[NSAboutPanelOptionApplicationVersion] = g_version;
    options[NSAboutPanelOptionVersion] = [NSString stringWithFormat:@"v%@", g_version];
    options[@"Copyright"] = @"Copyright © 2026 Fabian Schmieder\nGNU General Public License v3.0\nhttps://github.com/metaneutrons/devserial-mcp";
    if (g_icon) {
        options[NSAboutPanelOptionApplicationIcon] = g_icon;
    }

    [NSApp orderFrontStandardAboutPanelWithOptions:options];
    [NSApp activateIgnoringOtherApps:YES];
}

- (void)openHelp:(id)sender {
    (void)sender;
    [[NSWorkspace sharedWorkspace] openURL:[NSURL URLWithString:@"https://github.com/metaneutrons/devserial-mcp"]];
}

- (void)exportBuffer:(id)sender {
    (void)sender;
    g_exportRequested = YES;
}

- (void)openPort:(id)sender {
    (void)sender;
    g_openPortRequested = YES;
}

- (void)showPortSettings:(id)sender {
    (void)sender;
    g_portSettingsRequested = YES;
}

- (void)toggleConnect:(id)sender {
    (void)sender;
    g_toggleConnectRequested = YES;
}
@end

static DevSerialAboutHandler *g_aboutHandler = nil;

BOOL devserial_check_export_requested(void) {
    if (g_exportRequested) {
        g_exportRequested = NO;
        return YES;
    }
    return NO;
}

BOOL devserial_check_open_port_requested(void) {
    if (g_openPortRequested) {
        g_openPortRequested = NO;
        return YES;
    }
    return NO;
}

BOOL devserial_check_port_settings_requested(void) {
    if (g_portSettingsRequested) {
        g_portSettingsRequested = NO;
        return YES;
    }
    return NO;
}

BOOL devserial_check_toggle_connect_requested(void) {
    if (g_toggleConnectRequested) {
        g_toggleConnectRequested = NO;
        return YES;
    }
    return NO;
}

const char *devserial_get_clipboard_text(void) {
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        NSString *str = [pb stringForType:NSPasteboardTypeString];
        if (!str) return NULL;
        return strdup([str UTF8String]);
    }
}

void devserial_free_clipboard_text(const char *ptr) {
    if (ptr) {
        free((void *)ptr);
    }
}

void devserial_init_macos_app(const char *version_cstr, const uint8_t *icon_png_data, size_t icon_png_len) {
    @autoreleasepool {
        if (version_cstr && strlen(version_cstr) > 0) {
            g_version = [NSString stringWithUTF8String:version_cstr];
        }

        [[NSProcessInfo processInfo] setProcessName:@"devserial"];

        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];

        if (icon_png_data && icon_png_len > 0) {
            NSData *data = [NSData dataWithBytes:icon_png_data length:icon_png_len];
            g_icon = [[NSImage alloc] initWithData:data];
            if (g_icon) {
                [app setApplicationIconImage:g_icon];
            }
        }

        if (!g_aboutHandler) {
            g_aboutHandler = [[DevSerialAboutHandler alloc] init];
        }

        // Configure the macOS Main Menu
        NSMenu *mainMenu = [app mainMenu];
        if (!mainMenu) {
            mainMenu = [[NSMenu alloc] init];
            [app setMainMenu:mainMenu];
        }

        // ==========================================
        // 1. Application Menu (devserial)
        // ==========================================
        NSMenuItem *appMenuItem = nil;
        if ([mainMenu numberOfItems] > 0) {
            appMenuItem = [mainMenu itemAtIndex:0];
        } else {
            appMenuItem = [[NSMenuItem alloc] init];
            [mainMenu addItem:appMenuItem];
        }

        [appMenuItem setTitle:@"devserial"];

        NSMenu *appMenu = [appMenuItem submenu];
        if (!appMenu) {
            appMenu = [[NSMenu alloc] initWithTitle:@"devserial"];
            [appMenuItem setSubmenu:appMenu];
        } else {
            [appMenu setTitle:@"devserial"];
        }

        [appMenu removeAllItems];

        // 1.1 About devserial
        NSMenuItem *aboutItem = [[NSMenuItem alloc] initWithTitle:@"About devserial"
                                                           action:@selector(showAbout:)
                                                    keyEquivalent:@""];
        [aboutItem setTarget:g_aboutHandler];
        [appMenu addItem:aboutItem];

        [appMenu addItem:[NSMenuItem separatorItem]];

        // 1.2 Services
        NSMenuItem *servicesItem = [[NSMenuItem alloc] initWithTitle:@"Services" action:nil keyEquivalent:@""];
        NSMenu *servicesMenu = [[NSMenu alloc] initWithTitle:@"Services"];
        [servicesItem setSubmenu:servicesMenu];
        [app setServicesMenu:servicesMenu];
        [appMenu addItem:servicesItem];

        [appMenu addItem:[NSMenuItem separatorItem]];

        // 1.3 Hide devserial (Cmd+H)
        NSMenuItem *hideItem = [[NSMenuItem alloc] initWithTitle:@"Hide devserial"
                                                          action:@selector(hide:)
                                                   keyEquivalent:@"h"];
        [appMenu addItem:hideItem];

        // 1.4 Hide Others (Cmd+Opt+H)
        NSMenuItem *hideOthersItem = [[NSMenuItem alloc] initWithTitle:@"Hide Others"
                                                                action:@selector(hideOtherApplications:)
                                                         keyEquivalent:@"h"];
        [hideOthersItem setKeyEquivalentModifierMask:NSEventModifierFlagOption | NSEventModifierFlagCommand];
        [appMenu addItem:hideOthersItem];

        // 1.5 Show All
        NSMenuItem *showAllItem = [[NSMenuItem alloc] initWithTitle:@"Show All"
                                                             action:@selector(unhideAllApplications:)
                                                      keyEquivalent:@""];
        [appMenu addItem:showAllItem];

        [appMenu addItem:[NSMenuItem separatorItem]];

        // 1.6 Quit devserial (Cmd+Q)
        NSMenuItem *quitItem = [[NSMenuItem alloc] initWithTitle:@"Quit devserial"
                                                          action:@selector(terminate:)
                                                   keyEquivalent:@"q"];
        [appMenu addItem:quitItem];

        // Remove old extra menus so we re-create clean state
        while ([mainMenu numberOfItems] > 1) {
            [mainMenu removeItemAtIndex:1];
        }

        // ==========================================
        // 2. File Menu (Ablage)
        // ==========================================
        NSMenuItem *fileMenuItem = [[NSMenuItem alloc] initWithTitle:@"File" action:nil keyEquivalent:@""];
        NSMenu *fileMenu = [[NSMenu alloc] initWithTitle:@"File"];
        [fileMenuItem setSubmenu:fileMenu];
        [mainMenu addItem:fileMenuItem];

        // New Window... (Cmd+N)
        NSMenuItem *newWindowItem = [[NSMenuItem alloc] initWithTitle:@"New Window..."
                                                               action:@selector(openPort:)
                                                        keyEquivalent:@"n"];
        [newWindowItem setTarget:g_aboutHandler];
        [fileMenu addItem:newWindowItem];

        // Open Port... (Cmd+O)
        NSMenuItem *openPortItem = [[NSMenuItem alloc] initWithTitle:@"Open Port..."
                                                              action:@selector(openPort:)
                                                       keyEquivalent:@"o"];
        [openPortItem setTarget:g_aboutHandler];
        [fileMenu addItem:openPortItem];

        // Connect / Disconnect (Cmd+K)
        NSMenuItem *toggleConnectItem = [[NSMenuItem alloc] initWithTitle:@"Connect / Disconnect"
                                                                   action:@selector(toggleConnect:)
                                                            keyEquivalent:@"k"];
        [toggleConnectItem setTarget:g_aboutHandler];
        [fileMenu addItem:toggleConnectItem];

        // Port Settings... (Cmd+Shift+P)
        NSMenuItem *settingsItem = [[NSMenuItem alloc] initWithTitle:@"Port Settings..."
                                                              action:@selector(showPortSettings:)
                                                       keyEquivalent:@"P"];
        [settingsItem setKeyEquivalentModifierMask:NSEventModifierFlagCommand | NSEventModifierFlagShift];
        [settingsItem setTarget:g_aboutHandler];
        [fileMenu addItem:settingsItem];

        // Export Buffer... (Cmd+E)
        NSMenuItem *exportItem = [[NSMenuItem alloc] initWithTitle:@"Export Buffer..."
                                                            action:@selector(exportBuffer:)
                                                     keyEquivalent:@"e"];
        [exportItem setTarget:g_aboutHandler];
        [fileMenu addItem:exportItem];

        [fileMenu addItem:[NSMenuItem separatorItem]];

        // Close Window (Cmd+W)
        NSMenuItem *closeWindowItem = [[NSMenuItem alloc] initWithTitle:@"Close Window"
                                                                 action:@selector(performClose:)
                                                          keyEquivalent:@"w"];
        [fileMenu addItem:closeWindowItem];

        // ==========================================
        // 3. Edit Menu (Bearbeiten) - Routes to First Responder (WinitView / NSView)
        // ==========================================
        NSMenuItem *editMenuItem = [[NSMenuItem alloc] initWithTitle:@"Edit" action:nil keyEquivalent:@""];
        NSMenu *editMenu = [[NSMenu alloc] initWithTitle:@"Edit"];
        [editMenuItem setSubmenu:editMenu];
        [mainMenu addItem:editMenuItem];

        // Undo (Cmd+Z)
        NSMenuItem *undoItem = [[NSMenuItem alloc] initWithTitle:@"Undo"
                                                          action:@selector(undo:)
                                                   keyEquivalent:@"z"];
        [editMenu addItem:undoItem];

        // Redo (Cmd+Shift+Z)
        NSMenuItem *redoItem = [[NSMenuItem alloc] initWithTitle:@"Redo"
                                                          action:@selector(redo:)
                                                   keyEquivalent:@"Z"];
        [redoItem setKeyEquivalentModifierMask:NSEventModifierFlagCommand | NSEventModifierFlagShift];
        [editMenu addItem:redoItem];

        [editMenu addItem:[NSMenuItem separatorItem]];

        // Cut (Cmd+X)
        NSMenuItem *cutItem = [[NSMenuItem alloc] initWithTitle:@"Cut"
                                                         action:@selector(cut:)
                                                  keyEquivalent:@"x"];
        [editMenu addItem:cutItem];

        // Copy (Cmd+C)
        NSMenuItem *copyItem = [[NSMenuItem alloc] initWithTitle:@"Copy"
                                                          action:@selector(copy:)
                                                   keyEquivalent:@"c"];
        [editMenu addItem:copyItem];

        // Paste (Cmd+V)
        NSMenuItem *pasteItem = [[NSMenuItem alloc] initWithTitle:@"Paste"
                                                           action:@selector(paste:)
                                                    keyEquivalent:@"v"];
        [editMenu addItem:pasteItem];

        // Select All (Cmd+A)
        NSMenuItem *selectAllItem = [[NSMenuItem alloc] initWithTitle:@"Select All"
                                                               action:@selector(selectAll:)
                                                        keyEquivalent:@"a"];
        [editMenu addItem:selectAllItem];

        // ==========================================
        // 4. View Menu (Darstellung)
        // ==========================================
        NSMenuItem *viewMenuItem = [[NSMenuItem alloc] initWithTitle:@"View" action:nil keyEquivalent:@""];
        NSMenu *viewMenu = [[NSMenu alloc] initWithTitle:@"View"];
        [viewMenuItem setSubmenu:viewMenu];
        [mainMenu addItem:viewMenuItem];

        // Toggle Full Screen (Cmd+Ctrl+F)
        NSMenuItem *fullScreenItem = [[NSMenuItem alloc] initWithTitle:@"Toggle Full Screen"
                                                                action:@selector(toggleFullScreen:)
                                                         keyEquivalent:@"f"];
        [fullScreenItem setKeyEquivalentModifierMask:NSEventModifierFlagCommand | NSEventModifierFlagControl];
        [viewMenu addItem:fullScreenItem];

        // ==========================================
        // 5. Window Menu (Fenster)
        // ==========================================
        NSMenuItem *windowMenuItem = [[NSMenuItem alloc] initWithTitle:@"Window" action:nil keyEquivalent:@""];
        NSMenu *windowMenu = [[NSMenu alloc] initWithTitle:@"Window"];
        [windowMenuItem setSubmenu:windowMenu];
        [app setWindowsMenu:windowMenu];
        [mainMenu addItem:windowMenuItem];

        // Minimize (Cmd+M)
        NSMenuItem *minimizeItem = [[NSMenuItem alloc] initWithTitle:@"Minimize"
                                                              action:@selector(performMiniaturize:)
                                                       keyEquivalent:@"m"];
        [windowMenu addItem:minimizeItem];

        // Zoom
        NSMenuItem *zoomItem = [[NSMenuItem alloc] initWithTitle:@"Zoom"
                                                          action:@selector(performZoom:)
                                                   keyEquivalent:@""];
        [windowMenu addItem:zoomItem];

        [windowMenu addItem:[NSMenuItem separatorItem]];

        // Bring All to Front
        NSMenuItem *bringAllItem = [[NSMenuItem alloc] initWithTitle:@"Bring All to Front"
                                                              action:@selector(arrangeInFront:)
                                                       keyEquivalent:@""];
        [windowMenu addItem:bringAllItem];

        // ==========================================
        // 6. Help Menu (Hilfe)
        // ==========================================
        NSMenuItem *helpMenuItem = [[NSMenuItem alloc] initWithTitle:@"Help" action:nil keyEquivalent:@""];
        NSMenu *helpMenu = [[NSMenu alloc] initWithTitle:@"Help"];
        [helpMenuItem setSubmenu:helpMenu];
        [app setHelpMenu:helpMenu];
        [mainMenu addItem:helpMenuItem];

        NSMenuItem *docItem = [[NSMenuItem alloc] initWithTitle:@"devserial Documentation"
                                                         action:@selector(openHelp:)
                                                  keyEquivalent:@"?"];
        [docItem setTarget:g_aboutHandler];
        [helpMenu addItem:docItem];
    }
}
