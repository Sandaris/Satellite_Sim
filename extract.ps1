Add-Type -AssemblyName System.IO.Compression.FileSystem
$zipPath = "c:\Users\User\Documents\APU\RTS\Assignment\Satellite_Sim\Satellite_GCS_AI_Coding_Instructions.docx"
$zip = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
$entry = $zip.GetEntry("word/document.xml")
$stream = $entry.Open()
$reader = New-Object IO.StreamReader($stream)
$xml = $reader.ReadToEnd()
$reader.Close()
$stream.Close()
$zip.Dispose()

$text = $xml -replace '<w:p\b[^>]*>', "`n"
$text = $text -replace '<[^>]+>', ''
Write-Output $text
