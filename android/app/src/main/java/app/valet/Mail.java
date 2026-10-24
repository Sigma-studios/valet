package app.valet;

import android.app.AlarmManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.os.SystemClock;
import android.util.Log;
import android.webkit.CookieManager;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.IOException;
import java.net.HttpURLConnection;
import java.net.URL;

/**
 * New mail, told. The window can't receive Web Push as a browser does, so
 * every quarter of an hour or so the app asks valet what came into the
 * inboxes since it last looked (/mail/new.json) and shows it as valet words
 * it: one notification per message, or one for several.
 */
final class Mail {
    private static final String TAG = "valet";
    private static final String CHANNEL = "mail";
    // Android lets it come up to three quarters later: between 10 and 17 minutes.
    private static final long EVERY = 10 * 60_000L;

    private Mail() {}

    /** From the window: looks every quarter of an hour, and now if it never has (to learn where valet's list is). */
    static void start(Context c) {
        Context app = c.getApplicationContext();
        if (!firstLook(app)) schedule(app);
    }

    /** Logged in: the first look, now, if there wasn't one yet. Returns whether there wasn't. */
    static boolean firstLook(Context c) {
        Context app = c.getApplicationContext();
        if (MainActivity.prefs(app).contains("mail_after")) return false;
        new Thread(() -> check(app)).start();
        return true;
    }

    /** The next look, in a quarter of an hour or so; in Doze too (Android allows one every ten minutes at most). */
    static void schedule(Context c) {
        Intent i = new Intent(c, MailReceiver.class).setAction(MailReceiver.CHECK);
        PendingIntent pi = PendingIntent.getBroadcast(c, 0, i, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        c.getSystemService(AlarmManager.class).setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, SystemClock.elapsedRealtime() + EVERY, pi);
    }

    /** Blocks: asks valet, shows what came in (unless valet's window is in front), then schedules the next look. */
    static synchronized void check(Context c) {
        SharedPreferences p = MainActivity.prefs(c);
        String server = p.getString("server", null);
        if (server == null) return;
        try {
            long after = p.getLong("mail_after", -1);
            HttpURLConnection h = connect(server + "/mail/new.json" + (after >= 0 ? "?after=" + after : ""), server);
            int code = h.getResponseCode();
            // Logged out: nothing to tell until logged in again.
            if (code == 401) return;
            if (code != 200) throw new IOException("HTTP " + code);
            JSONObject r = new JSONObject(Alarms.read(h.getInputStream()));
            JSONArray notes = r.getJSONArray("notes");
            if (!MainActivity.shown) {
                for (int i = 0; i < notes.length(); i++) show(c, notes.getJSONObject(i));
            }
            p.edit().putLong("mail_after", r.getLong("last")).apply();
        } catch (Exception e) {
            Log.w(TAG, "mail not checked: " + e);
        } finally {
            schedule(c);
        }
    }

    private static HttpURLConnection connect(String url, String server) throws IOException {
        HttpURLConnection h = (HttpURLConnection) new URL(url).openConnection();
        String cookie = CookieManager.getInstance().getCookie(server);
        if (cookie != null) h.setRequestProperty("Cookie", cookie);
        h.setConnectTimeout(15_000);
        h.setReadTimeout(15_000);
        return h;
    }

    private static void show(Context c, JSONObject n) {
        NotificationManager nm = c.getSystemService(NotificationManager.class);
        NotificationChannel ch = new NotificationChannel(CHANNEL, "New mail", NotificationManager.IMPORTANCE_DEFAULT);
        ch.setDescription("Mail come into your inboxes");
        nm.createNotificationChannel(ch);
        String tag = n.optString("tag", "mail");
        String body = n.optString("body");
        Intent open = new Intent(c, MainActivity.class).putExtra("page", n.optString("url", "/inbox")).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        Notification.Builder b = new Notification.Builder(c, CHANNEL)
            .setSmallIcon(R.drawable.ic_mail)
            .setContentTitle(n.optString("title"))
            .setContentText(body.split("\n", 2)[0])
            .setStyle(new Notification.BigTextStyle().bigText(body))
            .setCategory(Notification.CATEGORY_EMAIL)
            .setShowWhen(true)
            .setAutoCancel(true)
            .setContentIntent(PendingIntent.getActivity(c, tag.hashCode(), open, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
        if (!n.optString("account").isEmpty()) b.setSubText(n.optString("account"));
        if (!n.isNull("message")) {
            Intent read = new Intent(c, MailReceiver.class).setAction(MailReceiver.READ).putExtra("message", n.optLong("message")).putExtra("tag", tag);
            PendingIntent pi = PendingIntent.getBroadcast(c, tag.hashCode(), read, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
            b.addAction(new Notification.Action.Builder(null, "Mark as read", pi).build());
        }
        nm.notify(tag, 0, b.build());
    }

    /** Blocks: the notification goes, valet marks the message read. */
    static void markRead(Context c, long message, String tag) {
        c.getSystemService(NotificationManager.class).cancel(tag, 0);
        String server = MainActivity.prefs(c).getString("server", null);
        if (server == null) return;
        try {
            HttpURLConnection h = connect(server + "/msg/" + message + "/seen", server);
            h.setRequestMethod("POST");
            h.setDoOutput(true);
            h.getOutputStream().close();
            int code = h.getResponseCode();
            if (code / 100 != 2) throw new IOException("HTTP " + code);
        } catch (Exception e) {
            Log.w(TAG, "not marked read: " + e);
        }
    }
}
