# Self-made window for the hands-free acceptance run (never touches the
# user's apps). Everything the acceptance script needs to verify an effect
# lives here and is reported through UIA and a JSON state file:
#   txtInput   multiline TextBox (type/paste/chords; UIA ValuePattern)
#   pnlCanvas  drawing panel: records every mouse stroke in screen pixels
#   btnTarget  click target: records the screen position of each click
#   btnShowLater / btnHideLater  make lblReady appear/disappear after 800 ms
#   btnPopup   opens an owned popup after -PopupDelayMs (takes the foreground)
#   closing with unsaved text asks "Save changes?" (Save / Don't save / Cancel)
# It is shown without activation and pushed to the bottom of the z-order.
# -RetitleAfterMs simulates an app that is still initializing: the title is
# "<Title> (loading)" and a control is added when the delay expires.
# The clipboard text present at start-up is restored when the window
# closes normally if the test changed it (ctrl+c); its content never leaves
# the app.
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File hf_test_app.ps1
#        -Title SosaAcceptance-<tag> -StateFile <path.json>
#        [-X 40 -Y 40] [-Seconds 900] [-RetitleAfterMs 0] [-PopupDelayMs 1500]
#        [-Minimized]
param(
    [Parameter(Mandatory = $true)][string]$Title,
    [Parameter(Mandatory = $true)][string]$StateFile,
    [int]$X = 40,
    [int]$Y = 40,
    [int]$Seconds = 900,
    [int]$RetitleAfterMs = 0,
    [int]$PopupDelayMs = 1500,
    [switch]$Minimized
)
$ErrorActionPreference = 'Stop'
Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Windows.Forms;

public class SosaSketch : Panel {
    public List<List<Point>> Strokes = new List<List<Point>>();
    List<Point> current;
    Bitmap ink;
    public event EventHandler StrokeEnded;
    public SosaSketch() {
        DoubleBuffered = true;
        BackColor = Color.White;
        BorderStyle = BorderStyle.FixedSingle;
    }
    void EnsureInk() {
        if (ink == null || ink.Width != ClientSize.Width || ink.Height != ClientSize.Height) {
            var b = new Bitmap(Math.Max(1, ClientSize.Width), Math.Max(1, ClientSize.Height));
            using (var g = Graphics.FromImage(b)) { g.Clear(Color.White); if (ink != null) g.DrawImage(ink, 0, 0); }
            ink = b;
        }
    }
    public int PointCount() { int n = 0; foreach (var s in Strokes) n += s.Count; return n; }
    void Track(MouseEventArgs e) {
        var p = PointToScreen(e.Location);
        if (current.Count > 0) {
            var q = PointToClient(current[current.Count - 1]);
            EnsureInk();
            using (var g = Graphics.FromImage(ink))
            using (var pen = new Pen(Color.Black, 3)) { g.DrawLine(pen, q, e.Location); }
            Invalidate();
        }
        current.Add(p);
    }
    protected override void OnMouseDown(MouseEventArgs e) {
        base.OnMouseDown(e);
        if (e.Button != MouseButtons.Left) return;
        current = new List<Point>();
        Strokes.Add(current);
        Track(e);
    }
    protected override void OnMouseMove(MouseEventArgs e) {
        base.OnMouseMove(e);
        if (current != null && (e.Button & MouseButtons.Left) != 0) Track(e);
    }
    protected override void OnMouseUp(MouseEventArgs e) {
        base.OnMouseUp(e);
        if (current == null || e.Button != MouseButtons.Left) return;
        Track(e);
        current = null;
        AccessibleName = "Canvas strokes=" + Strokes.Count + " points=" + PointCount();
        if (StrokeEnded != null) StrokeEnded(this, EventArgs.Empty);
    }
    protected override void OnPaint(PaintEventArgs e) {
        base.OnPaint(e);
        EnsureInk();
        e.Graphics.DrawImage(ink, 0, 0);
    }
}

public class SosaConfirm : Form {
    public string Choice = "cancel";
    public SosaConfirm(string title) {
        Text = title + " - Save changes?";
        FormBorderStyle = FormBorderStyle.FixedDialog;
        MinimizeBox = false; MaximizeBox = false; ShowInTaskbar = false;
        StartPosition = FormStartPosition.CenterParent;
        ClientSize = new Size(380, 110);
        var q = new Label { Name = "lblQuestion", Text = "Save changes to the test text?", Location = new Point(12, 14), Size = new Size(350, 22) };
        var save = new Button { Name = "btnSave", Text = "Save", Location = new Point(12, 60), Size = new Size(110, 32) };
        var dont = new Button { Name = "btnDontSave", Text = "Don't save", Location = new Point(132, 60), Size = new Size(110, 32) };
        var cancel = new Button { Name = "btnCancel", Text = "Cancel", Location = new Point(252, 60), Size = new Size(110, 32) };
        save.Click += (s, e) => { Choice = "save"; Close(); };
        dont.Click += (s, e) => { Choice = "dont_save"; Close(); };
        cancel.Click += (s, e) => { Choice = "cancel"; Close(); };
        CancelButton = cancel;
        Controls.AddRange(new Control[] { q, save, dont, cancel });
    }
}

public class SosaAcceptanceForm : Form {
    [DllImport("user32.dll")]
    static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    protected override bool ShowWithoutActivation { get { return true; } }

    string title, stateFile, savedClipboard, closedBy;
    bool dirty, forceClose, confirming, readyVisible, popupShown;
    int targetClicks, events;
    Point lastTargetClick = new Point(int.MinValue, int.MinValue);
    List<string> keys = new List<string>();
    int copiedLength = -1;
    bool copiedMatches;
    TextBox input;
    SosaSketch canvas;
    Label status, ready;

    public SosaAcceptanceForm(string title, string stateFile, int x, int y, int retitleMs, int popupMs, bool minimized) {
        this.title = title;
        this.stateFile = stateFile;
        Text = retitleMs > 0 ? title + " (loading)" : title;
        StartPosition = FormStartPosition.Manual;
        Location = new Point(x, y);
        ClientSize = new Size(660, 460);
        if (minimized) WindowState = FormWindowState.Minimized;
        try { if (Clipboard.ContainsText()) savedClipboard = Clipboard.GetText(); } catch { }

        input = new TextBox { Name = "txtInput", AccessibleName = "Input", Multiline = true, AcceptsReturn = true, AcceptsTab = true,
                              WordWrap = false, ScrollBars = ScrollBars.Both, Location = new Point(10, 10), Size = new Size(500, 120) };
        input.TextChanged += (s, e) => { dirty = true; Save(); };
        input.KeyDown += (s, e) => {
            if (e.Control || e.Alt) {
                keys.Add((e.Control ? "ctrl+" : "") + (e.Alt ? "alt+" : "") + (e.Shift ? "shift+" : "") + e.KeyCode.ToString().ToLowerInvariant());
                Save();
            }
        };
        input.KeyUp += (s, e) => {
            if (e.Control && e.KeyCode == Keys.C) {
                BeginInvoke(new Action(() => {
                    try {
                        var c = Clipboard.ContainsText() ? Clipboard.GetText() : null;
                        copiedLength = c == null ? -1 : c.Length;
                        copiedMatches = c != null && c == input.SelectedText;
                    } catch { copiedLength = -2; }
                    Save();
                }));
            }
        };
        canvas = new SosaSketch { Name = "pnlCanvas", AccessibleName = "Canvas strokes=0 points=0", Location = new Point(10, 140), Size = new Size(500, 300) };
        canvas.StrokeEnded += (s, e) => Save();
        var target = new Button { Name = "btnTarget", Text = "Target", Location = new Point(520, 140), Size = new Size(130, 60) };
        target.MouseUp += (s, e) => {
            if (e.Button != MouseButtons.Left) return;
            targetClicks++;
            lastTargetClick = target.PointToScreen(e.Location);
            status.Text = "target " + targetClicks;
            Save();
        };
        var showLater = new Button { Name = "btnShowLater", Text = "Show later", Location = new Point(520, 210), Size = new Size(130, 30) };
        var hideLater = new Button { Name = "btnHideLater", Text = "Hide later", Location = new Point(520, 245), Size = new Size(130, 30) };
        var popup = new Button { Name = "btnPopup", Text = "Popup", Location = new Point(520, 280), Size = new Size(130, 30) };
        ready = new Label { Name = "lblReady", Text = "Ready", Location = new Point(520, 320), Size = new Size(130, 22), Visible = false };
        status = new Label { Name = "lblStatus", Text = "idle", Location = new Point(520, 350), Size = new Size(130, 22) };
        showLater.Click += (s, e) => Later(800, () => { ready.Visible = true; readyVisible = true; Save(); });
        hideLater.Click += (s, e) => Later(800, () => { ready.Visible = false; readyVisible = false; Save(); });
        popup.Click += (s, e) => Later(popupMs, () => {
            var p = new Form { Text = title + " Popup", StartPosition = FormStartPosition.Manual, ClientSize = new Size(260, 80),
                               Location = new Point(Location.X + 80, Location.Y + 80), ShowInTaskbar = false };
            p.Show(this);
            p.Activate();
            popupShown = true;
            Save();
        });
        Controls.AddRange(new Control[] { input, canvas, target, showLater, hideLater, popup, ready, status });
        if (retitleMs > 0) {
            Later(retitleMs, () => {
                Text = title;
                Controls.Add(new Label { Name = "lblLoaded", Text = "loaded", Location = new Point(520, 380), Size = new Size(130, 22) });
                Save();
            });
        }
        Shown += (s, e) => { SetWindowPos(Handle, new IntPtr(1), 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0010); Save(); };
        FormClosing += OnClosing;
        FormClosed += (s, e) => {
            if (closedBy == null) closedBy = "close";
            Save();
            // Only when the test changed it: never rewrite (and downgrade)
            // a clipboard the run did not touch.
            try {
                if (savedClipboard != null && (!Clipboard.ContainsText() || Clipboard.GetText() != savedClipboard))
                    Clipboard.SetText(savedClipboard);
            } catch { }
        };
    }

    void Later(int ms, Action a) {
        var t = new Timer { Interval = Math.Max(1, ms) };
        t.Tick += (s, e) => { t.Stop(); t.Dispose(); a(); };
        t.Start();
    }

    public void ForceClose(string by) { forceClose = true; closedBy = by; Close(); }

    void OnClosing(object sender, FormClosingEventArgs e) {
        // Multiline edits get no EN_CHANGE for WM_SETTEXT (UIA set_value):
        // any text counts as unsaved.
        if (forceClose || !(dirty || input.Text.Length > 0)) return;
        e.Cancel = true;
        if (confirming) return;
        confirming = true;
        // Shown after the close request returns: a modal loop inside
        // FormClosing would block a UIA WindowPattern.Close caller.
        BeginInvoke(new Action(() => {
            string choice;
            using (var d = new SosaConfirm(title)) { d.ShowDialog(this); choice = d.Choice; }
            confirming = false;
            if (choice == "cancel") { closedBy = null; Save(); return; }
            ForceClose(choice);
        }));
    }

    static string J(string s) {
        if (s == null) return "null";
        var b = new StringBuilder("\"");
        foreach (char c in s) {
            if (c == '"') b.Append("\\\"");
            else if (c == '\\') b.Append("\\\\");
            else if (c < 0x20) b.Append("\\u").Append(((int)c).ToString("x4"));
            else b.Append(c);
        }
        return b.Append('"').ToString();
    }

    void Save() {
        events++;
        var b = new StringBuilder("{");
        b.Append("\"title\":").Append(J(title));
        b.Append(",\"window_text\":").Append(J(Text));
        b.Append(",\"pid\":").Append(System.Diagnostics.Process.GetCurrentProcess().Id);
        b.Append(",\"hwnd\":").Append(IsHandleCreated ? Handle.ToInt64() : 0);
        b.Append(",\"events\":").Append(events);
        b.Append(",\"text\":").Append(J(input.Text));
        b.Append(",\"dirty\":").Append(dirty || input.Text.Length > 0 ? "true" : "false");
        b.Append(",\"selection_length\":").Append(input.SelectionLength);
        b.Append(",\"keys\":[");
        for (int i = 0; i < keys.Count; i++) b.Append(i > 0 ? "," : "").Append(J(keys[i]));
        b.Append("],\"copied_length\":").Append(copiedLength);
        b.Append(",\"copied_matches_selection\":").Append(copiedMatches ? "true" : "false");
        b.Append(",\"strokes\":[");
        for (int i = 0; i < canvas.Strokes.Count; i++) {
            b.Append(i > 0 ? "," : "").Append("[");
            var st = canvas.Strokes[i];
            for (int k = 0; k < st.Count; k++) b.Append(k > 0 ? "," : "").Append("[").Append(st[k].X).Append(",").Append(st[k].Y).Append("]");
            b.Append("]");
        }
        b.Append("],\"target_clicks\":").Append(targetClicks);
        b.Append(",\"target_click_screen\":");
        if (targetClicks > 0) b.Append("[").Append(lastTargetClick.X).Append(",").Append(lastTargetClick.Y).Append("]"); else b.Append("null");
        b.Append(",\"ready_visible\":").Append(readyVisible ? "true" : "false");
        b.Append(",\"popup_shown\":").Append(popupShown ? "true" : "false");
        b.Append(",\"closed_by\":").Append(J(closedBy));
        b.Append("}");
        var tmp = stateFile + ".tmp";
        for (int i = 0; i < 10; i++) {
            try {
                File.WriteAllText(tmp, b.ToString(), new UTF8Encoding(false));
                if (File.Exists(stateFile)) File.Replace(tmp, stateFile, null); else File.Move(tmp, stateFile);
                return;
            } catch { System.Threading.Thread.Sleep(15); }
        }
    }
}
'@
[System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($StateFile)) | Out-Null
Add-Type -Namespace SosaDpi -Name Native -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();'
[SosaDpi.Native]::SetProcessDPIAware() | Out-Null
[System.Windows.Forms.Application]::EnableVisualStyles()
$form = New-Object SosaAcceptanceForm -ArgumentList $Title, $StateFile, $X, $Y, $RetitleAfterMs, $PopupDelayMs, $Minimized.IsPresent
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = $Seconds * 1000
$timer.Add_Tick({ $form.ForceClose('ttl') })
$timer.Start()
[System.Windows.Forms.Application]::Run($form)
