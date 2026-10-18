#include <xtl.h>
#include <winsockx.h>
#include <string>
#include <vector>
#include <algorithm>
#include <stdio.h>
#include <stdarg.h>
#include "AtgDevice.h"
#include "AtgFont.h"
#include "Embedded.h"

namespace ATG { D3DDevice* g_pd3dDevice = NULL; }
static std::wstring Wide(const std::string& text);
static const D3DCOLOR ORANGE=0xffff8700, WHITE=0xffeeeeee, MUTED=0xffaaaaaa;
class StoreScreen {
    ATG::Font font;
    struct TextRun { float x,y; D3DCOLOR color; std::wstring text; };
    std::vector<TextRun> text;
    struct RectRun { D3DRECT rect; D3DCOLOR color; };
    std::vector<RectRun> rectangles;
    bool suspended;
    D3DCOLOR background, foreground;
public:
    StoreScreen():suspended(false) {}
    HRESULT Create(const char* fontPath,D3DCOLOR bg,D3DCOLOR fg) {
        background=bg; foreground=fg;
        D3DPRESENT_PARAMETERS params={0};
        params.BackBufferWidth=1280; params.BackBufferHeight=720;
        params.BackBufferFormat=D3DFMT_A8R8G8B8; params.BackBufferCount=1;
        params.SwapEffect=D3DSWAPEFFECT_DISCARD;
        params.PresentationInterval=D3DPRESENT_INTERVAL_ONE;
        LPDIRECT3D9 direct=Direct3DCreate9(D3D_SDK_VERSION);
        HRESULT result=direct->CreateDevice(0,D3DDEVTYPE_HAL,NULL,D3DCREATE_HARDWARE_VERTEXPROCESSING,
            &params,(::D3DDevice**)&ATG::g_pd3dDevice);
        if(FAILED(result)) return result;
        result=font.Create(fontPath); font.SetScaleFactors(1.5f,1.5f); return result;
    }
    void Clear() { text.clear(); rectangles.clear(); }
    void Progress(unsigned percent) {
        RectRun track; track.rect.x1=64; track.rect.y1=420;
        track.rect.x2=1088; track.rect.y2=438; track.color=0xff343434;
        rectangles.push_back(track);
        percent=(std::min)(percent,100u);
        if(percent>0) {
            RectRun fill=track; fill.rect.x2=64+(1024*percent)/100; fill.color=ORANGE;
            rectangles.push_back(fill);
        }
        char label[16]; sprintf_s(label,"%u%%",percent);
        Text(1100,414,label,ORANGE);
    }
    void Text(float x,float y,const std::string& value,D3DCOLOR color=WHITE) {
        TextRun run; run.x=x; run.y=y; run.color=color; run.text=Wide(value); text.push_back(run);
    }
    void Paragraph(float y,const std::string& value,D3DCOLOR color=WHITE,int maxLines=5) {
        std::wstring remaining=Wide(value);
        for(int line=0;!remaining.empty() && line<maxLines;++line,y+=28) {
            size_t count=remaining.size(); float width,height;
            do { font.GetTextExtent(remaining.substr(0,count).c_str(),&width,&height); }
            while(width>1120 && --count>0);
            if(count<remaining.size()) {
                size_t space=remaining.rfind(L' ',count);
                if(space!=std::wstring::npos && space>0) count=space;
            }
            TextRun run; run.x=64; run.y=y; run.color=color; run.text=remaining.substr(0,count); text.push_back(run);
            remaining.erase(0,count);
            while(!remaining.empty() && remaining[0]==L' ') remaining.erase(0,1);
        }
    }
    void Render() {
        if(suspended) ATG::g_pd3dDevice->Resume();
        ATG::g_pd3dDevice->Clear(0,NULL,D3DCLEAR_TARGET,background,1.0f,0);
        for(size_t i=0;i<rectangles.size();++i)
            ATG::g_pd3dDevice->Clear(1,&rectangles[i].rect,D3DCLEAR_TARGET,rectangles[i].color,1.0f,0);
        font.Begin();
        for(size_t i=0;i<text.size();++i)
            font.DrawText(text[i].x,text[i].y,text[i].color,text[i].text.c_str(),ATGFONT_TRUNCATED,1152.0f);
        font.End();
        ATG::g_pd3dDevice->Present(NULL,NULL,NULL,NULL);
        ATG::g_pd3dDevice->Suspend(); suspended=true;
    }
};
static StoreScreen screen;
// Flush each checkpoint so a startup failure can be located after a reboot.
// This diagnostic contains no connection addresses or credentials.
static void StartupStatus(const char* phase,bool reset=false) {
    FILE* file=NULL;
    if(fopen_s(&file,"game:\\startup-status.txt",reset?"wb":"ab")==0 && file) {
        fprintf(file,"%lu %s\n",GetTickCount(),phase); fflush(file); fclose(file);
    }
}
static std::string server, token, pending, response;
static unsigned short serverPort=36061;
static volatile LONG busy = 0;
static bool succeeded = false;
static DWORD userIndex = 0;
struct Row { std::string key, label; };
static std::vector<Row> rows;
static std::string heading, gameId, currentAction;
static unsigned selected = 0, hostIndex = 0;
static int progressPercent=-1;
static std::string progressDetail;
enum Page { HOME, SEARCH, HOSTS, DRIVES, PROGRESS, NOTICE, SETUP, CONNECTION_ERROR };
static Page page = HOME, requestedPage = HOME;
static Page afterConnection=HOME;
static bool paired=false, connectionReady=false, exitAllowed=true;
static void Draw();

static std::string Json(const std::string& value) {
    std::string out = "\"";
    for (size_t i=0; i<value.size(); ++i) {
        unsigned char c = value[i];
        if (c=='"' || c=='\\') { out += '\\'; out += c; }
        else if (c < 32) { char escaped[8]; sprintf_s(escaped,"\\u%04x",c); out += escaped; }
        else out += c;
    }
    return out + "\"";
}

static std::wstring Wide(const std::string& text) {
    int size = MultiByteToWideChar(CP_UTF8, 0, text.c_str(), -1, NULL, 0);
    if (size<=0) return L"";
    std::vector<WCHAR> buffer(size);
    MultiByteToWideChar(CP_UTF8,0,text.c_str(),-1,&buffer[0],size);
    return &buffer[0];
}

static bool Keyboard(const WCHAR* title, bool password, std::string& result) {
    WCHAR buffer[256] = {0}; XOVERLAPPED overlapped = {0};
    DWORD code = XShowKeyboardUI(userIndex, password ? VKBD_LATIN_PASSWORD : VKBD_DEFAULT,
        L"", title, password ? L"Use your dashboard FTP password, not your Microsoft account password." : L"", buffer, 256, &overlapped);
    if (code == ERROR_IO_PENDING) {
        while (!XHasOverlappedIoCompleted(&overlapped)) { screen.Render(); Sleep(16); }
        code = XGetOverlappedExtendedError(&overlapped);
    }
    if (code != ERROR_SUCCESS || !buffer[0]) return false;
    char utf8[1024];
    WideCharToMultiByte(CP_UTF8,0,buffer,-1,utf8,sizeof(utf8),NULL,NULL);
    result = utf8; SecureZeroMemory(buffer,sizeof(buffer)); SecureZeroMemory(utf8,sizeof(utf8));
    return true;
}

static bool SendAll(SOCKET sock, const std::string& data) {
    size_t sent=0;
    while(sent<data.size()) { int n=send(sock,data.data()+sent,(int)(data.size()-sent),0); if(n<=0)return false; sent+=n; }
    return true;
}

static DWORD WINAPI RequestThread(LPVOID) {
    succeeded=false;
    response="Cannot reach your Store worker. Keep your PC on and check its address.";
    SOCKET sock=socket(AF_INET,SOCK_STREAM,IPPROTO_TCP);
    if(sock!=INVALID_SOCKET) {
        // Xbox direct LAN sockets: see mojobojo/PublicXboxStuff AllocLogger sample.
        BOOL plainSocket=TRUE;
        setsockopt(sock,SOL_SOCKET,0x5801,(const char*)&plainSocket,sizeof(plainSocket));
        DWORD timeout=180000;
        setsockopt(sock,SOL_SOCKET,SO_RCVTIMEO,(const char*)&timeout,sizeof(timeout));
        setsockopt(sock,SOL_SOCKET,SO_SNDTIMEO,(const char*)&timeout,sizeof(timeout));
        sockaddr_in addr={0}; addr.sin_family=AF_INET; addr.sin_port=htons(serverPort); addr.sin_addr.s_addr=inet_addr(server.c_str());
        if(addr.sin_addr.s_addr!=INADDR_NONE && connect(sock,(sockaddr*)&addr,sizeof(addr))==0) {
            char length[32]; sprintf_s(length,"%u",(unsigned)pending.size());
            std::string request="POST /v1/console HTTP/1.0\r\nHost: "+server+"\r\nAuthorization: Bearer "+token+
                "\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: "+length+"\r\n\r\n"+pending;
            if(SendAll(sock,request)) {
                std::string wire; char buffer[4096]; int n;
                while((n=recv(sock,buffer,sizeof(buffer),0))>0 && wire.size()<1024*1024) wire.append(buffer,n);
                size_t split=wire.find("\r\n\r\n");
                if(split!=std::string::npos) { response=wire.substr(split+4); succeeded=wire.find(" 200 ")<wire.find("\r\n"); }
            }
            std::fill(request.begin(),request.end(),0);
        }
        closesocket(sock);
    }
    std::fill(pending.begin(),pending.end(),0); pending.clear();
    FILE* statusFile=NULL;
    if(fopen_s(&statusFile,"game:\\connection-status.txt","wb")==0 && statusFile) {
        fputs(succeeded?"Worker connection verified\n":"Worker request failed\n",statusFile); fclose(statusFile);
    }
    InterlockedExchange(&busy,2);
    return 0;
}

static void Request(const char* action, const std::string& value, Page next, const std::string& extra="") {
    currentAction=action; requestedPage=next;
    pending="{\"action\":"+Json(action)+",\"value\":"+Json(value)+extra+"}";
    InterlockedExchange(&busy,1);
    HANDLE worker=CreateThread(NULL,0,RequestThread,NULL,0,NULL);
    if(worker) CloseHandle(worker); else { response="Could not start request."; succeeded=false; InterlockedExchange(&busy,2); }
}

static void Home() {
    page=HOME; heading="Choose an option:"; selected=0; rows.clear();
    const char* names[]={"1. Store","2. Exit"};
    for(int i=0;i<(exitAllowed?2:1);++i) { Row row; row.label=names[i]; rows.push_back(row); }
}

static void ConnectionNotice(bool loginRequired) {
    connectionReady=false;
    page=loginRequired?SETUP:CONNECTION_ERROR; selected=0; rows.clear();
    heading=loginRequired?"Enter your dashboard FTP details once. They will be remembered automatically.":response;
    Row row; row.label=loginRequired?"Enter connection details":"Retry connection"; rows.push_back(row);
    row.label="Continue to main menu"; rows.push_back(row);
}

static void Credentials() {
    std::string username,password;
    if(Keyboard(L"Dashboard FTP username",false,username) && Keyboard(L"Dashboard FTP password",true,password))
        Request("credentials",username,NOTICE,",\"password\":"+Json(password));
    std::fill(password.begin(),password.end(),0);
}

static void CheckConnection(Page next) {
    afterConnection=next;
    Request(next==DRIVES?"drives":"connection","",next);
}

static void ReadRows() {
    rows.clear(); selected=0;
    progressPercent=-1; progressDetail.clear();
    size_t start=0,end=response.find('\n'); heading=response.substr(0,end);
    while(end!=std::string::npos) {
        start=end+1; end=response.find('\n',start);
        std::string line=response.substr(start,end==std::string::npos?std::string::npos:end-start);
        size_t tab=line.find('\t');
        if(tab!=std::string::npos) {
            Row row; row.key=line.substr(0,tab); row.label=line.substr(tab+1);
            if(page==PROGRESS && row.key=="@progress") progressPercent=(std::max)(0,(std::min)(100,atoi(row.label.c_str())));
            else if(page==PROGRESS && row.key=="@detail") progressDetail=row.label;
            else rows.push_back(row);
        }
    }
    if(page==PROGRESS) {
        exitAllowed=heading=="Idle";
        for(unsigned i=0;i<rows.size();++i) if(rows[i].key=="store") exitAllowed=true;
    }
}

static void Draw() {
    screen.Clear();
    // Same 56-column banner, colour and indentation as the PC terminal.
    screen.Text(64,48,"========================================================",ORANGE);
    screen.Text(64,90,"                     Digital 360");
    screen.Text(64,132,"========================================================",ORANGE);
    screen.Text(64,176,"  Archives: prepared on your PC");
    screen.Text(64,204,connectionReady?"  Xbox FTP: Connected (details remembered)":
        busy==1?"  Xbox FTP: Checking connection":"  Xbox FTP: Not connected");
    screen.Text(64,254,exitAllowed?"How it works: use D-pad and A. Press B to return or exit.":"Installation in progress. Keep Digital 360 open.");
    if(busy==1 && currentAction!="activity") {
        screen.Text(64,310,"Working...",ORANGE);
        screen.Paragraph(354,currentAction=="connection"?"Checking your saved Xbox connection...":
            currentAction=="credentials"?"Testing and saving your connection details...":"Checking the host and all required files.");
    }
    else {
        if(page==PROGRESS) screen.Paragraph(310,heading,WHITE,3);
        else if(page==NOTICE || page==SETUP || page==CONNECTION_ERROR) screen.Paragraph(310,heading);
        else screen.Text(64,310,heading);
        unsigned first=(selected/8)*8;
        float top=(page==NOTICE || page==SETUP || page==CONNECTION_ERROR || page==PROGRESS)?470.0f:366.0f;
        if(page==PROGRESS && progressPercent>=0) {
            screen.Progress((unsigned)progressPercent);
            screen.Paragraph(448,progressDetail,MUTED,1);
            top=500.0f;
        }
        for(unsigned i=first;i<rows.size() && i<first+8;++i) {
            screen.Text(64,top+(i-first)*28,(i==selected?"  > ":"    ")+rows[i].label,i==selected?ORANGE:WHITE);
        }
        if(rows.size()>8) {
            char count[64]; sprintf_s(count,"%u-%u of %u",first+1,(unsigned)(std::min)(rows.size(),(size_t)(first+8)),(unsigned)rows.size());
            screen.Text(64,600,count,MUTED);
        }
    }
    screen.Text(64,604,exitAllowed?"Select an option with D-pad and A.":"Downloading, extracting and uploading automatically. Please wait.",MUTED);
    screen.Text(64,632,"Keep your PC and Xbox on until installation finishes.",MUTED);
}

static bool LoadConfig() {
    FILE* file=NULL;
    if(fopen_s(&file,"game:\\worker.ini","rb")!=0 || !file) return false;
    char line[512];
    while(fgets(line,sizeof(line),file)) {
        std::string text=line; while(!text.empty() && (text[text.size()-1]=='\r'||text[text.size()-1]=='\n'))text.erase(text.size()-1);
        if(text.find("host=")==0)server=text.substr(5);
        if(text.find("port=")==0) { int port=atoi(text.substr(5).c_str()); if(port<1 || port>65535) { fclose(file); return false; } serverPort=(unsigned short)port; }
        if(text.find("token=")==0)token=text.substr(6);
    }
    fclose(file);
    return inet_addr(server.c_str())!=INADDR_NONE && token.size()>=32 && token.find_first_of("\r\n")==std::string::npos;
}

static void ExitToDashboard() {
    if(!exitAllowed) { Request("activity","",PROGRESS); return; }
    if(paired) Request("leave","",NOTICE);
    else XLaunchNewImage(XLAUNCH_KEYWORD_DEFAULT_APP,NULL);
}

void __cdecl main() {
    StartupStatus("startup",true);
    XNetStartupParams net={0}; net.cfgSizeOfStruct=sizeof(net); net.cfgFlags=XNET_STARTUP_BYPASS_SECURITY;
    XNetStartup(&net); WSADATA wsa; WSAStartup(MAKEWORD(2,2),&wsa);
    if(!WriteSection("FONT","game:\\font.xpr")) { StartupStatus("font extraction failed"); return; }
    StartupStatus("font extracted");
    if(FAILED(screen.Create("game:\\font.xpr",0xff0c0c0c,WHITE))) { StartupStatus("renderer creation failed"); return; }
    StartupStatus("renderer ready; artwork import starting");
    InstallEmbeddedArtwork();
    StartupStatus("artwork import returned");
    Home();
    paired=LoadConfig();
    if(!paired) { page=NOTICE; heading="Worker pairing is missing. Reinstall this Xbox app from your configured PC."; rows.clear(); }
    else {
        CheckConnection(HOME);
    }
    Draw(); WORD previous=0; DWORD lastRefresh=GetTickCount();
    screen.Render();
    StartupStatus("first menu frame rendered");
    for(;;) {
        if(busy==2) {
            InterlockedExchange(&busy,0); page=succeeded?requestedPage:NOTICE;
            if(succeeded && currentAction=="leave") XLaunchNewImage(XLAUNCH_KEYWORD_DEFAULT_APP,NULL);
            if(!succeeded && (currentAction=="connection" || currentAction=="drives" || currentAction=="credentials")) {
                bool loginRequired=response.find("FTP_LOGIN_REQUIRED:")!=std::string::npos;
                ConnectionNotice(loginRequired);
                if(currentAction=="credentials" && loginRequired) heading="Login failed. Check your dashboard details and try again.";
                Draw();
                if(loginRequired && currentAction!="credentials") { Credentials(); Draw(); previous=0xffff; }
                continue;
            }
            if(succeeded && currentAction=="credentials") {
                CheckConnection(afterConnection); Draw(); continue;
            }
            if(succeeded && (currentAction=="queue" || currentAction=="retry" || currentAction=="connection")) {
                if(currentAction=="queue" || currentAction=="retry") exitAllowed=false;
                connectionReady=true; Request("activity","",PROGRESS); Draw(); continue;
            }
            if(succeeded && (currentAction=="connection" || currentAction=="drives")) connectionReady=true;
            unsigned previousSelection=selected;
            if(page==HOME) Home(); else ReadRows();
            if(page==PROGRESS && heading=="Idle") Home();
            else if(page==PROGRESS && previousSelection<rows.size()) selected=previousSelection;
            Draw(); lastRefresh=GetTickCount();
        }
        if(page==PROGRESS && !busy && GetTickCount()-lastRefresh>1000) { Request("activity","",PROGRESS); lastRefresh=GetTickCount(); }
        XINPUT_STATE input={0}; bool connected=false;
        for(DWORD i=0;i<4;++i) if(XInputGetState(i,&input)==ERROR_SUCCESS) { userIndex=i; connected=true; break; }
        WORD pressed=connected ? input.Gamepad.wButtons & ~previous : 0; previous=input.Gamepad.wButtons;
        if(!busy && pressed) {
            if(pressed&XINPUT_GAMEPAD_B) { if(!exitAllowed) { Request("activity","",PROGRESS); } else if(page==HOME) ExitToDashboard(); else Home(); Draw(); }
            else if(pressed&XINPUT_GAMEPAD_DPAD_DOWN) { if(selected+1<rows.size())++selected; Draw(); }
            else if(pressed&XINPUT_GAMEPAD_DPAD_UP) { if(selected)--selected; Draw(); }
            else if((pressed&XINPUT_GAMEPAD_A) && selected<rows.size()) {
                if(page==HOME) {
                    if(selected==1) ExitToDashboard();
                    else if(!paired) { page=NOTICE; heading="Worker pairing is missing. Reinstall this Xbox app from your configured PC."; rows.clear(); }
                    else if(selected==0) { std::string query; if(Keyboard(L"Search games",false,query))Request("search",query,SEARCH); }
                } else if(page==SETUP || page==CONNECTION_ERROR) {
                    if(selected==1) Home();
                    else if(page==SETUP) Credentials();
                    else CheckConnection(afterConnection);
                } else if(page==SEARCH) { gameId=rows[selected].key; Request("options",gameId,HOSTS); }
                else if(page==HOSTS) { hostIndex=atoi(rows[selected].key.c_str()); CheckConnection(DRIVES); }
                else if(page==DRIVES) {
                    char index[32]; sprintf_s(index,"%u",hostIndex);
                    Request("queue",gameId,NOTICE,",\"index\":"+std::string(index)+",\"drive\":"+Json(rows[selected].key));
                } else if(page==PROGRESS) {
                    if(rows[selected].key=="exit") ExitToDashboard();
                    else if(rows[selected].key=="store") Home();
                    else if(rows[selected].key.find("retry:")==0) Request("retry",rows[selected].key.substr(6),PROGRESS);
                }
                Draw(); previous=0xffff;
            }
        }
        screen.Render(); Sleep(16);
    }
}
