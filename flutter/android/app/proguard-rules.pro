# Native methods and callback names are resolved by the Rust/JNI bridge.
-keep class com.airplayreceiver.desktop.nativebridge.FusionPlayNative { *; }
-keep interface com.airplayreceiver.desktop.nativebridge.NativeCallback { *; }
-keep class * implements com.airplayreceiver.desktop.nativebridge.NativeCallback { *; }
