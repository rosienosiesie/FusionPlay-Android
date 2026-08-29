plugins {
    id("com.android.application")
}

android {
    namespace = "com.fusionplay.miplaylyricsprobe"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.fusionplay.miplaylyricsprobe"
        minSdk = 21
        targetSdk = 36
        versionCode = 1
        versionName = "1.0"
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
