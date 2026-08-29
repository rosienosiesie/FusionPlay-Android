package com.airplayreceiver.desktop.backend

import java.io.IOException
import java.nio.charset.StandardCharsets
import java.nio.file.AtomicMoveNotSupportedException
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.nio.file.StandardOpenOption
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerializationException
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.put

class SettingsStore(
    val settingsPath: Path = defaultSettingsPath(),
    private val json: Json = DEFAULT_JSON,
) {
    private val mutex = Mutex()

    suspend fun load(): AppSettings = loadWithStatus().settings

    suspend fun loadWithStatus(): SettingsLoadResult = mutex.withLock {
        withContext(Dispatchers.IO) {
            readUnlocked()
        }
    }

    suspend fun save(settings: AppSettings): AppSettings = mutex.withLock {
        val validated = settings.validated()
        withContext(Dispatchers.IO) {
            writeUnlocked(validated)
        }
        validated
    }

    suspend fun update(transform: (AppSettings) -> AppSettings): AppSettings = mutex.withLock {
        val current = withContext(Dispatchers.IO) {
            readUnlocked().settings
        }
        val updated = transform(current).validated()
        withContext(Dispatchers.IO) {
            writeUnlocked(updated)
        }
        updated
    }

    private fun readUnlocked(): SettingsLoadResult {
        if (!Files.exists(settingsPath)) {
            return SettingsLoadResult(AppSettings(), existed = false)
        }

        val text = try {
            String(Files.readAllBytes(settingsPath), StandardCharsets.UTF_8)
        } catch (exception: IOException) {
            throw IOException("Unable to read settings from $settingsPath.", exception)
        }

        val settings = try {
            decode(text).validated()
        } catch (exception: Exception) {
            if (exception !is SerializationException && exception !is IllegalArgumentException) {
                throw exception
            }
            throw IOException(
                "Settings JSON is invalid; the existing file was left unchanged: $settingsPath.",
                exception,
            )
        }
        return SettingsLoadResult(settings, existed = true)
    }

    private fun writeUnlocked(settings: AppSettings) {
        val parent = settingsPath.toAbsolutePath().normalize().parent
            ?: throw IOException("Settings path has no parent directory: $settingsPath")
        Files.createDirectories(parent)

        val temporary = Files.createTempFile(parent, "settings-", ".tmp")
        try {
            val encoded = json.encodeToString(JsonElement.serializer(), encode(settings))
            Files.write(
                temporary,
                encoded.toByteArray(StandardCharsets.UTF_8),
                StandardOpenOption.WRITE,
                StandardOpenOption.TRUNCATE_EXISTING,
            )
            try {
                Files.move(
                    temporary,
                    settingsPath,
                    StandardCopyOption.ATOMIC_MOVE,
                    StandardCopyOption.REPLACE_EXISTING,
                )
            } catch (_: AtomicMoveNotSupportedException) {
                Files.move(
                    temporary,
                    settingsPath,
                    StandardCopyOption.REPLACE_EXISTING,
                )
            }
        } finally {
            Files.deleteIfExists(temporary)
        }
    }

    private fun decode(text: String): AppSettings {
        val root = json.parseToJsonElement(text) as? JsonObject
            ?: throw IllegalArgumentException("Settings root must be a JSON object.")
        return AppSettings(
            schemaVersion = root["schemaVersion"]?.jsonPrimitive?.intOrNull
                ?: AppSettings.CURRENT_SCHEMA_VERSION,
            receiverName = root["receiverName"]?.jsonPrimitive?.contentOrNull,
            outputDeviceId = null,
            xiaomiNetworkAdapterId = null,
            startupEnabled = root["startupEnabled"]?.jsonPrimitive?.booleanOrNull ?: true,
            autoWakeEnabled = root["autoWakeEnabled"]?.jsonPrimitive?.booleanOrNull ?: true,
            advancedEffectsEnabled =
                root["advancedEffectsEnabled"]?.jsonPrimitive?.booleanOrNull ?: false,
            miPlayEnabled = root["miPlayEnabled"]?.jsonPrimitive?.booleanOrNull ?: true,
            miPlayDeviceIdentity = MiPlayDeviceIdentity.fromPersistedValue(
                root["miPlayDeviceIdentity"]?.jsonPrimitive?.contentOrNull,
            ),
            airPlayEnabled = root["airPlayEnabled"]?.jsonPrimitive?.booleanOrNull ?: true,
            dlnaEnabled = root["dlnaEnabled"]?.jsonPrimitive?.booleanOrNull ?: true,
        )
    }

    private fun encode(settings: AppSettings): JsonObject = buildJsonObject {
        put("schemaVersion", settings.schemaVersion)
        settings.receiverName?.let { put("receiverName", it) }
        put("startupEnabled", settings.startupEnabled)
        put("autoWakeEnabled", settings.autoWakeEnabled)
        put("advancedEffectsEnabled", settings.advancedEffectsEnabled)
        put("miPlayEnabled", settings.miPlayEnabled)
        put(
            "miPlayDeviceIdentity",
            settings.miPlayDeviceIdentity.persistedValue,
        )
        put("airPlayEnabled", settings.airPlayEnabled)
        put("dlnaEnabled", settings.dlnaEnabled)
    }

    companion object {
        private val DEFAULT_JSON = Json {
            prettyPrint = true
            isLenient = false
        }

        fun defaultSettingsPath(): Path =
            localAppDataDirectory()
                .resolve(APP_DIRECTORY_NAME)
                .resolve(SETTINGS_FILE_NAME)

        fun localAppDataDirectory(): Path {
            runCatching { return AndroidPaths.filesDirectory() }
            val environmentPath = System.getenv("LOCALAPPDATA")
                ?.trim()
                ?.takeIf(String::isNotEmpty)
            if (environmentPath != null) {
                return Path.of(environmentPath)
            }

            val userHome = System.getProperty("user.home")
                ?.trim()
                ?.takeIf(String::isNotEmpty)
                ?: throw IllegalStateException(
                    "Neither AndroidPaths, LOCALAPPDATA nor user.home is available.",
                )
            return Path.of(userHome)
        }

        const val APP_DIRECTORY_NAME = "AirPlayReceiver"
        const val SETTINGS_FILE_NAME = "settings.json"
    }
}
