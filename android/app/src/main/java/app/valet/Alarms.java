package app.valet;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.os.Bundle;
import android.os.SystemClock;
import android.util.Log;
import android.webkit.CookieManager;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.net.URLEncoder;
import java.util.TimeZone;

/**
 * The alarms to ring: valet says which (/calendar/alarms.json, the alerts of
 * patterns marked to ring, a week ahead) and the phone sets them, so they go
 * off at their minute, offline too. The last list is kept for restarts
 * without a network.
 */
final class Alarms {
    private static final String TAG = "valet";
    private static final long HOUR = 3600_000L;
    private static long last;

    private Alarms() {}

    /** In the background; at most every ten minutes unless `now`. */
    static void refreshSoon(Context c, boolean now) {
        long t = System.currentTimeMillis();
        if (!now && t - last < 10 * 60_000L) return;
        last = t;
        Context app = c.getApplicationContext();
        new Thread(() -> refresh(app)).start();
    }

    /** Blocks: asks valet, then sets the alarms (the kept ones if valet can't be reached). */
    static void refresh(Context c) {
        SharedPreferences p = MainActivity.prefs(c);
        String server = p.getString("server", null);
        if (server == null) return;
        try {
            String url = server + "/calendar/alarms.json?tz=" + URLEncoder.encode(TimeZone.getDefault().getID(), "UTF-8");
            HttpURLConnection h = (HttpURLConnection) new URL(url).openConnection();
            String cookie = CookieManager.getInstance().getCookie(server);
            if (cookie != null) h.setRequestProperty("Cookie", cookie);
            h.setConnectTimeout(15_000);
            h.setReadTimeout(15_000);
            int code = h.getResponseCode();
            if (code == 401) {
                // Logged out: nothing rings.
                p.edit().remove("alarms").apply();
                schedule(c, new JSONArray());
                return;
            }
            if (code != 200) throw new IOException("HTTP " + code);
            JSONArray list = new JSONObject(read(h.getInputStream())).getJSONArray("alarms");
            p.edit().putString("alarms", list.toString()).apply();
            schedule(c, list);
        } catch (Exception e) {
            Log.w(TAG, "alarms not refreshed: " + e);
            try {
                schedule(c, new JSONArray(p.getString("alarms", "[]")));
            } catch (JSONException ignored) {
            }
        } finally {
            periodic(c);
        }
    }

    static String read(InputStream in) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        for (int n; (n = in.read(buf)) > 0; ) out.write(buf, 0, n);
        return out.toString("UTF-8");
    }

    /** Replaces the alarms set before with `list` (those still to come). */
    static synchronized void schedule(Context c, JSONArray list) throws JSONException {
        AlarmManager am = c.getSystemService(AlarmManager.class);
        SharedPreferences p = MainActivity.prefs(c);
        for (int i = 0, n = p.getInt("scheduled", 0); i < n; i++) am.cancel(ring(c, i, new Bundle(), 0));
        long now = System.currentTimeMillis();
        int n = 0;
        for (int i = 0; i < list.length() && n < 100; i++) {
            JSONObject a = list.getJSONObject(i);
            long at = a.getLong("at") * 1000;
            if (at <= now) continue;
            // The event, to say what the alarm is for: its start, an instant or (all day) a date.
            Bundle about = new Bundle();
            about.putString("title", a.optString("title"));
            Object start = a.opt("start");
            if (start instanceof Number) about.putLong("start", ((Number) start).longValue() * 1000);
            if (start instanceof String) about.putString("day", (String) start);
            set(c, at, ring(c, n, about, at));
            n++;
        }
        p.edit().putInt("scheduled", n).apply();
        Log.i(TAG, n + " alarms set");
    }

    static PendingIntent ring(Context c, int code, Bundle about, long at) {
        Intent i = new Intent(c, AlarmReceiver.class).setAction(AlarmReceiver.RING).putExtras(about).putExtra("at", at);
        return PendingIntent.getBroadcast(c, code, i, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    /** At its minute, waking the phone (the clock in the status bar shows it). */
    static void set(Context c, long at, PendingIntent pi) {
        AlarmManager am = c.getSystemService(AlarmManager.class);
        Intent show = new Intent(c, MainActivity.class).putExtra("page", "/calendar");
        PendingIntent showing = PendingIntent.getActivity(c, 0, show, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        try {
            am.setAlarmClock(new AlarmManager.AlarmClockInfo(at, showing), pi);
        } catch (SecurityException e) {
            // Exact alarms refused (Android 12, permission off): within minutes.
            am.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, at, pi);
        }
    }

    /** Events change elsewhere too: look again every few hours. */
    private static void periodic(Context c) {
        Intent i = new Intent(c, RefreshReceiver.class).setAction(RefreshReceiver.PERIODIC);
        PendingIntent pi = PendingIntent.getBroadcast(c, 1, i, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        c.getSystemService(AlarmManager.class).setInexactRepeating(AlarmManager.ELAPSED_REALTIME_WAKEUP, SystemClock.elapsedRealtime() + 3 * HOUR, 3 * HOUR, pi);
    }
}
