package app.valet;

import android.Manifest;
import android.app.Activity;
import android.app.AlarmManager;
import android.app.AlertDialog;
import android.app.DownloadManager;
import android.app.NotificationManager;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.provider.Settings;
import android.text.InputType;
import android.view.WindowInsets;
import android.webkit.CookieManager;
import android.webkit.JavascriptInterface;
import android.webkit.URLUtil;
import android.webkit.ValueCallback;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.Toast;

/**
 * valet's own site, in a window of its own: mail, calendar, settings are the
 * web pages (and work offline as they do in a browser). The app adds alarms
 * that ring, for the alert patterns marked so (see {@link Alarms}).
 */
public class MainActivity extends Activity {
    static final String PREFS = "valet";
    private static final int FILES = 1;

    private WebView web;
    private ValueCallback<Uri[]> picking;
    private String server;

    static SharedPreferences prefs(Context c) {
        return c.getSharedPreferences(PREFS, MODE_PRIVATE);
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        web = new WebView(this);
        FrameLayout frame = new FrameLayout(this);
        frame.addView(web);
        setContentView(frame);
        // Drawn edge to edge (Android 15+): the page stays clear of the bars and the keyboard.
        if (Build.VERSION.SDK_INT >= 30) {
            frame.setOnApplyWindowInsetsListener((v, insets) -> {
                android.graphics.Insets b = insets.getInsets(WindowInsets.Type.systemBars() | WindowInsets.Type.ime());
                v.setPadding(b.left, b.top, b.right, b.bottom);
                return WindowInsets.CONSUMED;
            });
        }
        WebSettings s = web.getSettings();
        s.setJavaScriptEnabled(true);
        s.setDomStorageEnabled(true);
        s.setMixedContentMode(WebSettings.MIXED_CONTENT_NEVER_ALLOW);
        s.setUserAgentString(s.getUserAgentString() + " ValetApp/1");
        CookieManager.getInstance().setAcceptCookie(true);
        web.addJavascriptInterface(new Bridge(), "ValetApp");
        web.setWebViewClient(new Client());
        web.setWebChromeClient(new Chrome());
        web.setDownloadListener((url, agent, disposition, mime, length) -> download(url, agent, disposition, mime));

        server = prefs(this).getString("server", null);
        if (server == null) {
            askServer();
        } else if (state != null) {
            web.restoreState(state);
        } else {
            web.loadUrl(start(getIntent()));
        }
        askPermissions();
    }

    /** The page to open: the one an alarm points to, or the mail. */
    private String start(Intent intent) {
        String page = intent != null ? intent.getStringExtra("page") : null;
        return server + (page != null ? page : "/inbox");
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        if (server != null && intent.getStringExtra("page") != null) web.loadUrl(start(intent));
    }

    @Override
    protected void onSaveInstanceState(Bundle out) {
        super.onSaveInstanceState(out);
        web.saveState(out);
    }

    private void askServer() {
        EditText input = new EditText(this);
        input.setInputType(InputType.TYPE_TEXT_VARIATION_URI);
        input.setHint("https://mail.example.com");
        input.setSingleLine();
        new AlertDialog.Builder(this)
            .setTitle("Your valet")
            .setMessage("The address of your valet, as in your browser.")
            .setView(input)
            .setCancelable(false)
            .setPositiveButton("Open", (d, w) -> {
                String url = input.getText().toString().trim().replaceAll("/+$", "");
                if (!url.startsWith("http")) url = "https://" + url;
                server = url;
                prefs(this).edit().putString("server", url).apply();
                web.loadUrl(server + "/inbox");
            })
            .show();
    }

    /** Notifications, exact alarms and full-screen alarms, once. */
    private void askPermissions() {
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 2);
        }
        SharedPreferences p = prefs(this);
        if (Build.VERSION.SDK_INT >= 31 && Build.VERSION.SDK_INT < 33 && !getSystemService(AlarmManager.class).canScheduleExactAlarms()) {
            startActivity(new Intent(Settings.ACTION_REQUEST_SCHEDULE_EXACT_ALARM, Uri.parse("package:" + getPackageName())));
        }
        if (Build.VERSION.SDK_INT >= 34 && !getSystemService(NotificationManager.class).canUseFullScreenIntent() && !p.getBoolean("asked_full_screen", false)) {
            p.edit().putBoolean("asked_full_screen", true).apply();
            new AlertDialog.Builder(this)
                .setTitle("Alarms on the lock screen")
                .setMessage("To show an alarm over the lock screen, valet needs the permission for full-screen notifications. Without it, alarms still ring, as a notification.")
                .setPositiveButton("Allow", (d, w) -> startActivity(new Intent(Settings.ACTION_MANAGE_APP_USE_FULL_SCREEN_INTENT, Uri.parse("package:" + getPackageName()))))
                .setNegativeButton("Not now", null)
                .show();
        }
    }

    private void download(String url, String agent, String disposition, String mime) {
        DownloadManager.Request r = new DownloadManager.Request(Uri.parse(url));
        r.addRequestHeader("Cookie", CookieManager.getInstance().getCookie(url));
        r.addRequestHeader("User-Agent", agent);
        r.setNotificationVisibility(DownloadManager.Request.VISIBILITY_VISIBLE_NOTIFY_COMPLETED);
        r.setDestinationInExternalPublicDir(Environment.DIRECTORY_DOWNLOADS, URLUtil.guessFileName(url, disposition, mime));
        getSystemService(DownloadManager.class).enqueue(r);
        Toast.makeText(this, "Downloading…", Toast.LENGTH_SHORT).show();
    }

    @Override
    public void onBackPressed() {
        if (web.canGoBack()) web.goBack();
        else super.onBackPressed();
    }

    @Override
    protected void onActivityResult(int code, int result, Intent data) {
        super.onActivityResult(code, result, data);
        if (code == FILES && picking != null) {
            picking.onReceiveValue(WebChromeClient.FileChooserParams.parseResult(result, data));
            picking = null;
        }
    }

    private class Client extends WebViewClient {
        @Override
        public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest req) {
            Uri u = req.getUrl();
            if (server != null && u.toString().startsWith(server)) return false;
            // Links in mail open in the browser.
            try {
                startActivity(new Intent(Intent.ACTION_VIEW, u));
            } catch (Exception e) {
                Toast.makeText(MainActivity.this, "No app opens " + u.getScheme() + " links", Toast.LENGTH_SHORT).show();
            }
            return true;
        }

        @Override
        public void onPageFinished(WebView view, String url) {
            CookieManager.getInstance().flush();
            // Logged in, and maybe changed patterns or events: alarms follow.
            if (url != null && !url.contains("/login")) Alarms.refreshSoon(MainActivity.this, url.contains("/patterns") || url.contains("/calendar"));
        }
    }

    private class Chrome extends WebChromeClient {
        @Override
        public boolean onShowFileChooser(WebView view, ValueCallback<Uri[]> callback, FileChooserParams params) {
            if (picking != null) picking.onReceiveValue(null);
            picking = callback;
            try {
                startActivityForResult(params.createIntent(), FILES);
            } catch (Exception e) {
                picking = null;
                return false;
            }
            return true;
        }
    }

    /** What the pages can ask of the app (`window.ValetApp`). */
    private class Bridge {
        @JavascriptInterface
        public void refreshAlarms() {
            Alarms.refreshSoon(MainActivity.this, true);
        }

        @JavascriptInterface
        public void changeServer() {
            runOnUiThread(MainActivity.this::askServer);
        }
    }
}
