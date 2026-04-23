#!/usr/bin/env python3
"""
Apply Android-specific patches to the Tauri-generated Android project.

Run from the project root AFTER `tauri android init`:
    python3 android-patches/apply_patches.py

What this script does:
  1. Finds the generated AndroidManifest.xml and injects all required permissions
     (RECORD_AUDIO, SYSTEM_ALERT_WINDOW, FOREGROUND_SERVICE, etc.) plus PiP
     config and the DbMeterService declaration.
  2. Finds MainActivity.kt and injects runtime permission requests, overlay
     permission flow, PiP entry, and the foreground-service starter.
  3. Creates DbMeterService.kt alongside MainActivity.kt so audio keeps
     running when the user goes to the home screen / switches apps.
"""

import os
import re
import sys

GEN_ANDROID = "src-tauri/gen/android"


# ── Helpers ───────────────────────────────────────────────────────────────────

def find_file(name: str) -> str | None:
    for root, _dirs, files in os.walk(GEN_ANDROID):
        if name in files:
            return os.path.join(root, name)
    return None


def read(path: str) -> str:
    with open(path, encoding="utf-8") as f:
        return f.read()


def write(path: str, content: str) -> None:
    with open(path, "w", encoding="utf-8") as f:
        f.write(content)
    print(f"  Wrote {path}")


def inject_before(content: str, marker: str, block: str) -> str:
    """Insert `block` immediately before the first occurrence of `marker`."""
    idx = content.find(marker)
    if idx == -1:
        raise ValueError(f"Marker not found: {marker!r}")
    return content[:idx] + block + content[idx:]


# ── 1. Patch AndroidManifest.xml ──────────────────────────────────────────────

def patch_manifest(path: str) -> None:
    print(f"Patching {path}...")
    content = read(path)

    permissions = [
        "android.permission.RECORD_AUDIO",
        "android.permission.MODIFY_AUDIO_SETTINGS",
        "android.permission.FOREGROUND_SERVICE",
        "android.permission.POST_NOTIFICATIONS",
        "android.permission.SYSTEM_ALERT_WINDOW",
    ]

    for perm in permissions:
        tag = f'<uses-permission android:name="{perm}" />'
        if tag not in content:
            content = content.replace(
                "    <application",
                f"    {tag}\n    <application",
                1,
            )

    feature = '<uses-feature android:name="android.hardware.microphone" android:required="true" />'
    if feature not in content:
        content = content.replace("    <application", f"    {feature}\n    <application", 1)

    # Enable Picture-in-Picture on the main activity
    if "supportsPictureInPicture" not in content:
        content = content.replace(
            'android:name=".MainActivity"',
            (
                'android:name=".MainActivity"\n'
                '            android:supportsPictureInPicture="true"\n'
                '            android:configChanges="screenSize|smallestScreenSize|screenLayout|orientation"'
            ),
        )

    # Declare the foreground service
    if "DbMeterService" not in content:
        service_xml = (
            "\n        <service\n"
            '            android:name=".DbMeterService"\n'
            '            android:exported="false" />'
        )
        content = content.replace("    </application>", service_xml + "\n    </application>")

    write(path, content)


# ── 2. Patch MainActivity.kt ──────────────────────────────────────────────────

IMPORTS_TO_ADD = """\
import android.app.PictureInPictureParams
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Rational
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat"""

ONCREATE_INJECTION = """\

        // Request microphone access (required for cpal/Oboe on Android)
        requestMicPermission()
        // Ask for SYSTEM_ALERT_WINDOW so the app can float over games
        requestOverlayPermission()
        // Start foreground service — keeps audio alive when app is minimised
        startDbMeterService()"""

HELPER_METHODS = """
    // ── Runtime permission helpers ────────────────────────────────────────────

    private fun requestMicPermission() {
        val perms = arrayOf(
            android.Manifest.permission.RECORD_AUDIO,
            android.Manifest.permission.MODIFY_AUDIO_SETTINGS,
        )
        val missing = perms.filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (missing.isNotEmpty()) {
            ActivityCompat.requestPermissions(this, missing.toTypedArray(), 1001)
        }
    }

    private fun requestOverlayPermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M && !Settings.canDrawOverlays(this)) {
            val intent = Intent(
                Settings.ACTION_MANAGE_OVERLAY_PERMISSION,
                Uri.parse("package:$packageName"),
            )
            startActivity(intent)
        }
    }

    private fun startDbMeterService() {
        val intent = Intent(this, DbMeterService::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent)
        } else {
            startService(intent)
        }
    }

    // ── Picture-in-Picture ────────────────────────────────────────────────────
    // Called when user presses the Home button — shrink to floating pip window.

    override fun onUserLeaveHint() {
        super.onUserLeaveHint()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            enterPictureInPictureMode(
                PictureInPictureParams.Builder()
                    .setAspectRatio(Rational(1, 1))
                    .build(),
            )
        }
    }
"""


def patch_main_activity(path: str) -> None:
    print(f"Patching {path}...")
    content = read(path)

    if "ActivityCompat" not in content:
        # Find last import line and add our imports after it
        last_import = max(
            (m.end() for m in re.finditer(r"^import .+$", content, re.MULTILINE)),
            default=0,
        )
        content = content[:last_import] + "\n" + IMPORTS_TO_ADD + content[last_import:]

    if "requestMicPermission" not in content:
        # Inject calls into onCreate after super.onCreate(...)
        content = re.sub(
            r"(super\.onCreate\(savedInstanceState\))",
            r"\1" + ONCREATE_INJECTION,
            content,
            count=1,
        )

    if "onUserLeaveHint" not in content:
        # Insert helper methods before the very last closing brace
        last_brace = content.rfind("}")
        content = content[:last_brace] + HELPER_METHODS + "\n" + content[last_brace:]

    write(path, content)


# ── 3. Create DbMeterService.kt ───────────────────────────────────────────────

def create_service(main_activity_path: str, package_name: str) -> None:
    service_path = os.path.join(os.path.dirname(main_activity_path), "DbMeterService.kt")
    print(f"Creating {service_path}...")

    content = f"""\
package {package_name}

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.os.Build
import android.os.IBinder

/**
 * Foreground service that keeps the dB Meter process alive when the user
 * switches to another app or presses Home.
 *
 * Android 8+ kills background processes after ~1 minute unless a foreground
 * service with a visible notification is running. This service satisfies that
 * requirement so the Rust audio thread (cpal/Oboe) keeps capturing from the
 * microphone continuously.
 *
 * To stop it: pull down the notification and tap "Stop", or call
 *   stopService(Intent(context, DbMeterService::class.java))
 * from MainActivity.
 */
class DbMeterService : Service() {{

    companion object {{
        const val CHANNEL_ID     = "db_meter_channel"
        const val NOTIFICATION_ID = 1
        const val ACTION_STOP    = "STOP"
    }}

    override fun onCreate() {{
        super.onCreate()
        createNotificationChannel()
        startForeground(NOTIFICATION_ID, buildNotification())
    }}

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {{
        if (intent?.action == ACTION_STOP) {{
            stopSelf()
            return START_NOT_STICKY
        }}
        return START_STICKY
    }}

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {{
        super.onDestroy()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {{
            stopForeground(STOP_FOREGROUND_REMOVE)
        }} else {{
            @Suppress("DEPRECATION")
            stopForeground(true)
        }}
    }}

    // ── Notification ──────────────────────────────────────────────────────────

    private fun createNotificationChannel() {{
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {{
            val channel = NotificationChannel(
                CHANNEL_ID,
                "dB Meter",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {{
                description = "dB Meter is measuring your microphone level"
                setSound(null, null)
                enableVibration(false)
            }}
            getSystemService(NotificationManager::class.java)
                .createNotificationChannel(channel)
        }}
    }}

    private fun buildNotification(): Notification {{
        // Tap notification → open the app
        val openPi = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).apply {{
                flags = Intent.FLAG_ACTIVITY_SINGLE_TOP
            }},
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        // "Stop" action in the notification
        val stopPi = PendingIntent.getService(
            this, 1,
            Intent(this, DbMeterService::class.java).apply {{ action = ACTION_STOP }},
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {{
            Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("dB Meter")
                .setContentText("Measuring microphone levels in background")
                .setSmallIcon(android.R.drawable.ic_btn_speak_now)
                .setContentIntent(openPi)
                .addAction(android.R.drawable.ic_delete, "Stop", stopPi)
                .setOngoing(true)
                .build()
        }} else {{
            @Suppress("DEPRECATION")
            android.app.Notification.Builder(this)
                .setContentTitle("dB Meter")
                .setContentText("Measuring microphone levels in background")
                .setSmallIcon(android.R.drawable.ic_btn_speak_now)
                .setContentIntent(openPi)
                .setOngoing(true)
                .build()
        }}
    }}
}}
"""
    write(service_path, content)


# ── Entry point ───────────────────────────────────────────────────────────────

def main() -> None:
    if not os.path.isdir(GEN_ANDROID):
        print(f"ERROR: {GEN_ANDROID!r} not found. Run `tauri android init` first.", file=sys.stderr)
        sys.exit(1)

    manifest_path = find_file("AndroidManifest.xml")
    main_activity_path = find_file("MainActivity.kt")

    if not manifest_path:
        print("ERROR: AndroidManifest.xml not found.", file=sys.stderr)
        sys.exit(1)
    if not main_activity_path:
        print("ERROR: MainActivity.kt not found.", file=sys.stderr)
        sys.exit(1)

    # Extract package name from MainActivity
    pkg_match = re.search(r"^package\s+(\S+)", read(main_activity_path), re.MULTILINE)
    if not pkg_match:
        print("ERROR: Could not determine package name from MainActivity.kt.", file=sys.stderr)
        sys.exit(1)
    package_name = pkg_match.group(1)
    print(f"Package: {package_name}")

    patch_manifest(manifest_path)
    patch_main_activity(main_activity_path)
    create_service(main_activity_path, package_name)

    print("\nAll patches applied successfully.")


if __name__ == "__main__":
    main()
