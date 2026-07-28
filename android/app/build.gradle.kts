plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.github.tymonoman.extraspace"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.tymonoman.extraspace"
        // The host refuses to talk to a mismatched app, so this is the number it
        // compares against when deciding whether to push a new APK.
        versionCode = 1
        versionName = "0.1.0"
        // MediaFormat.KEY_LOW_LATENCY needs 30; below that the decoder buffers
        // several frames and the whole latency budget is gone.
        minSdk = 30
        targetSdk = 35
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Debug-signed on purpose: the APK is sideloaded over adb by the host,
            // never published to a store, and a real signing key would be one more
            // thing standing between a user and a working setup.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }

    sourceSets["main"].java.srcDirs("src/main/kotlin")
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.activity:activity:1.9.3")
    implementation("androidx.appcompat:appcompat:1.7.0")
}
