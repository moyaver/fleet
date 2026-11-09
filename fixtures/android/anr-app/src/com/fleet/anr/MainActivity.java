// SPDX-License-Identifier: AGPL-3.0-only
package com.fleet.anr;

import android.app.Activity;
import android.os.Bundle;
import android.view.Gravity;
import android.widget.Button;
import android.widget.LinearLayout;

/**
 * Two buttons for the fixtures (issue #77). "Block main thread" blocks the main thread on tap, so
 * the system raises an ANR. "Crash app" throws on tap, so the app dies with a FATAL EXCEPTION.
 */
public class MainActivity extends Activity {
    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setGravity(Gravity.CENTER);
        Button block = new Button(this);
        block.setText("Block main thread");
        block.setAllCaps(false);
        block.setOnClickListener(v -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        });
        root.addView(block);
        Button crash = new Button(this);
        crash.setText("Crash app");
        crash.setAllCaps(false);
        crash.setOnClickListener(v -> {
            throw new RuntimeException("Fleet app crash");
        });
        root.addView(crash);
        setContentView(root);
    }
}
