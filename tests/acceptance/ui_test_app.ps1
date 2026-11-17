# Deterministic WinForms window for manual UI Automation tests of
# windows_list_windows / ui_snapshot / ui_action / capture (never touches the
# user's windows). Shown without activation and pushed to the bottom of
# the z-order so it does not steal focus or cover anything.
# Usage: powershell -NoProfile -File ui_test_app.ps1 -Title <unique> [-X 40 -Y 40]
param(
    [Parameter(Mandatory = $true)][string]$Title,
    [int]$X = 40,
    [int]$Y = 40,
    [int]$Seconds = 900
)
$ErrorActionPreference = 'Stop'
Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;
using System.Windows.Forms;

public class SosaTestForm : Form {
    [DllImport("user32.dll")]
    static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    protected override bool ShowWithoutActivation { get { return true; } }
    public Label Status;

    public SosaTestForm(string title, int x, int y) {
        Text = title;
        StartPosition = FormStartPosition.Manual;
        Location = new Point(x, y);
        ClientSize = new Size(520, 420);
        BackColor = Color.FromArgb(240, 240, 240);

        var button = new Button { Name = "btnHello", Text = "Say hello", Location = new Point(10, 10), Size = new Size(120, 30) };
        Status = new Label { Name = "lblStatus", Text = "idle", Location = new Point(140, 16), Size = new Size(200, 20) };
        button.Click += (s, e) => { Status.Text = "clicked"; };
        var text = new TextBox { Name = "txtName", Text = "initial", Location = new Point(10, 50), Size = new Size(200, 24) };
        var check = new CheckBox { Name = "chkOption", Text = "Enable option", Location = new Point(10, 84), Size = new Size(160, 24) };
        var combo = new ComboBox { Name = "cmbColor", DropDownStyle = ComboBoxStyle.DropDownList, Location = new Point(10, 116), Size = new Size(160, 24) };
        combo.Items.AddRange(new object[] { "Red", "Green", "Blue" });
        combo.SelectedIndex = 0;
        var list = new ListBox { Name = "lstFruit", Location = new Point(220, 50), Size = new Size(140, 80) };
        list.Items.AddRange(new object[] { "Apple", "Banana", "Cherry" });
        var tree = new TreeView { Name = "treeNodes", Location = new Point(370, 50), Size = new Size(140, 120) };
        var root = tree.Nodes.Add("Root");
        root.Nodes.Add("Child A");
        root.Nodes.Add("Child B");
        var memo = new TextBox { Name = "txtMemo", Multiline = true, ScrollBars = ScrollBars.Vertical, Location = new Point(10, 180), Size = new Size(300, 120) };
        var lines = new string[60];
        for (int i = 0; i < lines.Length; i++) lines[i] = "Line " + (i + 1);
        memo.Text = string.Join("\r\n", lines);
        var radioA = new RadioButton { Name = "radA", Text = "Mode A", Location = new Point(330, 190), Size = new Size(90, 24), Checked = true };
        var radioB = new RadioButton { Name = "radB", Text = "Mode B", Location = new Point(330, 220), Size = new Size(90, 24) };
        var panel = new Panel { Name = "pnlColour", BackColor = Color.FromArgb(30, 120, 200), Location = new Point(10, 320), Size = new Size(500, 90) };
        Controls.AddRange(new Control[] { button, Status, text, check, combo, list, tree, memo, radioA, radioB, panel });
        Shown += (s, e) => SetWindowPos(Handle, new IntPtr(1), 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0010);
    }
}
'@
$form = New-Object SosaTestForm -ArgumentList $Title, $X, $Y
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = $Seconds * 1000
$timer.Add_Tick({ $form.Close() })
$timer.Start()
[System.Windows.Forms.Application]::Run($form)
