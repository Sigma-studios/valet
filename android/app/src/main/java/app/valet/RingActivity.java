package app.valet;

import android.app.Activity;
import android.graphics.Color;
import android.os.Build;
import android.os.Bundle;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.WindowManager;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;

import java.text.DateFormat;
import java.util.Date;

/** An alarm, full screen (over the lock screen): Stop or Snooze. */
public class RingActivity extends Activity {
    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        if (Build.VERSION.SDK_INT < 27) {
            getWindow().addFlags(WindowManager.LayoutParams.FLAG_SHOW_WHEN_LOCKED | WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON);
        }
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        int id = getIntent().getIntExtra("id", 0);
        String title = getIntent().getStringExtra("title");
        long at = getIntent().getLongExtra("at", System.currentTimeMillis());

        LinearLayout box = new LinearLayout(this);
        box.setOrientation(LinearLayout.VERTICAL);
        box.setGravity(Gravity.CENTER);
        box.setPadding(48, 48, 48, 48);
        box.addView(text(DateFormat.getTimeInstance(DateFormat.SHORT).format(new Date(at)), 64));
        box.addView(text(title == null || title.isEmpty() ? "Alarm" : title, 26));
        Button stop = button("Stop");
        Button snooze = button("Snooze 10 min");
        stop.setOnClickListener(v -> send(AlarmReceiver.STOP, id, title, at));
        snooze.setOnClickListener(v -> send(AlarmReceiver.SNOOZE, id, title, at));
        box.addView(stop);
        box.addView(snooze);
        setContentView(box);
    }

    private void send(String action, int id, String title, long at) {
        try {
            AlarmReceiver.action(this, action, id, title, at).send();
        } catch (Exception ignored) {
        }
        finish();
    }

    private TextView text(String s, int sp) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextColor(Color.WHITE);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, sp);
        t.setGravity(Gravity.CENTER);
        t.setPadding(0, 12, 0, 12);
        return t;
    }

    private Button button(String s) {
        Button b = new Button(this);
        b.setText(s);
        b.setTextSize(TypedValue.COMPLEX_UNIT_SP, 20);
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT);
        lp.topMargin = 32;
        b.setLayoutParams(lp);
        return b;
    }
}
