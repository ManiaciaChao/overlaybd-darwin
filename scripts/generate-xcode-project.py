#!/usr/bin/env python3
"""Generate a small Xcode project with real ExtensionKit packaging/signing."""
from pathlib import Path
import plistlib
objects={}
def add(kind, **values):
    key=f'{len(objects)+1:024X}'
    objects[key]={'isa':kind,**values}
    return key
def configs(settings):
    ids=[add('XCBuildConfiguration',name=name,buildSettings={**settings,'SWIFT_OPTIMIZATION_LEVEL':'-Onone' if name=='Debug' else '-O','GCC_OPTIMIZATION_LEVEL':'0' if name=='Debug' else '2','OBD_RUST_PROFILE':'debug' if name=='Debug' else 'release'}) for name in ['Debug','Release']]
    return add('XCConfigurationList',buildConfigurations=ids,defaultConfigurationIsVisible=0,defaultConfigurationName='Debug')
def file(path,typ,source='<group>'):
    return add('PBXFileReference',path=path,lastKnownFileType=typ,sourceTree=source)
def phase(kind,files):
    return add(kind,buildActionMask=2147483647,files=files,runOnlyForDeploymentPostprocessing=0)
def build(ref): return add('PBXBuildFile',fileRef=ref)
hostsrcs=[file('OverlayBDHost/'+name,'sourcecode.swift') for name in ['Host.swift','Localization.swift','Backend.swift','LibraryModel.swift','LibraryView.swift','LayerGraph.swift','GraphViewportView.swift','RelationshipView.swift']]
localizations=add('PBXVariantGroup',name='Localizable.strings',children=[
    add('PBXFileReference',name=language,path=f'OverlayBDHost/{language}.lproj/Localizable.strings',lastKnownFileType='text.plist.strings',sourceTree='<group>')
    for language in ['en','zh-Hans']],sourceTree='<group>')
icon=file('OverlayBDHost/AppIcon.icon','folder.iconcomposer.icon')
extsrc=file('OverlayBDFSKit/Extension.swift','sourcecode.swift')
objc=file('OverlayBDFSKit/RawFileSystem.m','sourcecode.c.objc')
header=file('OverlayBDFSKit/RawFileSystem.h','sourcecode.c.h')
hostproduct=file('OverlayBDHost.app','wrapper.application','BUILT_PRODUCTS_DIR')
extproduct=file('OverlayBDFSKit.appex','wrapper.extensionkit-extension','BUILT_PRODUCTS_DIR')
products=add('PBXGroup',name='Products',children=[hostproduct,extproduct],sourceTree='<group>')
root=add('PBXGroup',children=[*hostsrcs,extsrc,objc,header,icon,localizations,products],sourceTree='<group>')
common={'SDKROOT':'macosx','MACOSX_DEPLOYMENT_TARGET':'26.0','CLANG_ENABLE_MODULES':'YES','CLANG_ENABLE_OBJC_ARC':'YES','SWIFT_VERSION':'5.0','CODE_SIGN_STYLE':'Automatic','CODE_SIGN_IDENTITY':'Apple Development','ENABLE_APP_SANDBOX':'YES','ENABLE_HARDENED_RUNTIME':'YES','PRODUCT_NAME':'$(TARGET_NAME)','GENERATE_INFOPLIST_FILE':'NO','ARCHS':'arm64','CURRENT_PROJECT_VERSION':'9','OTHER_SWIFT_FLAGS':['$(inherited)','-enable-upcoming-feature','ConciseMagicFile','-debug-prefix-map','$(SRCROOT)=/src/overlaybd-darwin/macos','-file-prefix-map','$(SRCROOT)=/src/overlaybd-darwin/macos'],'OTHER_CFLAGS':['$(inherited)','-ffile-prefix-map=$(SRCROOT)=/src/overlaybd-darwin/macos'],'MARKETING_VERSION':'0.1.0'}
extconf=configs({**common,'PRODUCT_BUNDLE_IDENTIFIER':'dev.overlaybd.mac.RawFS','INFOPLIST_FILE':'OverlayBDFSKit/Info.plist','CODE_SIGN_ENTITLEMENTS':'OverlayBDFSKit/Extension.entitlements','SWIFT_OBJC_BRIDGING_HEADER':'OverlayBDFSKit/RawFileSystem.h','OTHER_LDFLAGS':['$(SRCROOT)/../target/$(OBD_RUST_PROFILE)/libobd_runtime.a','-framework','FSKit','-framework','Security','-framework','SystemConfiguration','-liconv','-lc++'],'ENABLE_USER_SCRIPT_SANDBOXING':'NO','SKIP_INSTALL':'YES','LD_RUNPATH_SEARCH_PATHS':['$(inherited)','@executable_path/../Frameworks','@executable_path/../../../../Frameworks']})
engine=add('PBXShellScriptBuildPhase',name='Build Rust engine',buildActionMask=2147483647,files=[],inputPaths=[],outputPaths=['$(SRCROOT)/../target/$(OBD_RUST_PROFILE)/libobd_runtime.a'],runOnlyForDeploymentPostprocessing=0,shellPath='/bin/bash',shellScript='"$SRCROOT/../scripts/build-engine.sh"',alwaysOutOfDate='1')
ext=add('PBXNativeTarget',name='OverlayBDFSKit',productName='OverlayBDFSKit',productReference=extproduct,productType='com.apple.product-type.extensionkit-extension',buildConfigurationList=extconf,buildPhases=[engine,phase('PBXSourcesBuildPhase',[build(extsrc),build(objc)]),phase('PBXFrameworksBuildPhase',[]),phase('PBXResourcesBuildPhase',[])],buildRules=[],dependencies=[])
dep=add('PBXTargetDependency',target=ext)
embed=add('PBXBuildFile',fileRef=extproduct,settings={'ATTRIBUTES':['RemoveHeadersOnCopy']})
copy=add('PBXCopyFilesBuildPhase',name='Embed ExtensionKit Extensions',buildActionMask=2147483647,dstPath='$(EXTENSIONS_FOLDER_PATH)',dstSubfolderSpec=16,files=[embed],runOnlyForDeploymentPostprocessing=0)
helpers=add('PBXShellScriptBuildPhase',name='Bundle Rust control tools',buildActionMask=2147483647,files=[],inputPaths=[],outputPaths=[],runOnlyForDeploymentPostprocessing=0,shellPath='/bin/bash',shellScript='"$SRCROOT/../scripts/bundle-tools.sh"',alwaysOutOfDate='1')
hostconf=configs({**common,'ASSETCATALOG_COMPILER_APPICON_NAME':'AppIcon','PRODUCT_BUNDLE_IDENTIFIER':'dev.overlaybd.mac','INFOPLIST_FILE':'OverlayBDHost/Info.plist','CODE_SIGN_ENTITLEMENTS':'OverlayBDHost/Host.entitlements','ENABLE_APP_SANDBOX':'NO','ENABLE_USER_SCRIPT_SANDBOXING':'NO','ENABLE_HARDENED_RUNTIME':'YES','LD_RUNPATH_SEARCH_PATHS':['$(inherited)','@executable_path/../Frameworks']})
host=add('PBXNativeTarget',name='OverlayBDHost',productName='OverlayBDHost',productReference=hostproduct,productType='com.apple.product-type.application',buildConfigurationList=hostconf,buildPhases=[phase('PBXSourcesBuildPhase',[build(src) for src in hostsrcs]),phase('PBXFrameworksBuildPhase',[]),phase('PBXResourcesBuildPhase',[build(icon),build(localizations)]),copy,helpers],buildRules=[],dependencies=[dep])
project=add('PBXProject',attributes={'LastUpgradeCheck':'2700','BuildIndependentTargetsInParallel':'YES'},buildConfigurationList=configs({}),compatibilityVersion='Xcode 14.0',developmentRegion='en',hasScannedForEncodings=0,knownRegions=['en','zh-Hans','Base'],mainGroup=root,productRefGroup=products,projectDirPath='',projectRoot='',targets=[host,ext])
p=Path(__file__).resolve().parent.parent/'macos/OverlayBD.xcodeproj/project.pbxproj'
p.parent.mkdir(exist_ok=True)
p.write_bytes(plistlib.dumps({'archiveVersion':'1','classes':{},'objectVersion':'56','objects':objects,'rootObject':project},sort_keys=False))
