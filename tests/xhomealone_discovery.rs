use personal_downloaders::{Site, sites, sites::xhomealone};
use url::Url;

const TAG: &str = "https://xhomealone.com/tags/fixture-performer-000/";
const POST: &str =
    "https://xhomealone.com/videos/900004/fixture-performer-000-chaturbate-webcam-closeup-chat/";
const MEDIA: &str = "https://xhomealone.com/get_file/4/2a005f0857e2afde7b41c795af075bfebb80224ba2/900000/900004/900004.mp4/";

#[test]
fn tag_and_post_urls_are_normalized_and_scoped() {
    assert_eq!(sites::normalize(Site::XHomeAlone, TAG).unwrap(), TAG);
    for bad in [
        "http://xhomealone.com/tags/fixture-performer-000/",
        "https://xhomealone.com.evil.test/tags/fixture-performer-000/",
        "https://xhomealone.com/tags/fixture-performer-000/?sort=rating",
    ] {
        assert!(sites::normalize(Site::XHomeAlone, bad).is_err());
    }
    assert!(xhomealone::validate_media(&Url::parse(MEDIA).unwrap(), POST).is_ok());
    assert!(
        xhomealone::validate_media(
            &Url::parse(&MEDIA.replace("900004.mp4", "232414.mp4")).unwrap(),
            POST
        )
        .is_err()
    );
}

#[test]
fn only_declared_player_mp4_and_tag_catalog_links_are_accepted() {
    let html = format!(
        "<link rel='canonical' href='{TAG}'><div id='list_videos_common_videos_list_items'><a href='{POST}' title='clip'>clip</a></div>"
    );
    let posts = xhomealone::parse_catalog(&html, TAG).unwrap();
    assert_eq!(posts, vec![("900004".to_string(), POST.to_string())]);
    let player = format!(
        "<h1>Sample clip</h1><script>var flashvars = {{ video_id: '900004', video_url: 'function/0/{MEDIA}' }};</script>"
    );
    let f = xhomealone::parse_video(&player, POST, "fixture-performer-000").unwrap();
    assert_eq!(f.url, MEDIA);
    assert!(f.path.ends_with("Sample clip.mp4"));
    assert!(
        xhomealone::parse_video(
            &player.replace("900004.mp4", "999.mp4"),
            POST,
            "fixture-performer-000"
        )
        .is_err()
    );
}

#[test]
fn transport_accepts_tag_post_and_bound_mp4() {
    use personal_downloaders::transport::{SitePolicy, UrlPolicy};
    let policy = SitePolicy(Site::XHomeAlone);
    assert!(policy.check(&Url::parse(TAG).unwrap(), TAG).is_ok());
    assert!(policy.check(&Url::parse(POST).unwrap(), POST).is_ok());
    assert!(policy.check(&Url::parse(MEDIA).unwrap(), POST).is_ok());
    assert!(policy.check(&Url::parse(POST).unwrap(), TAG).is_err());
    assert!(policy.check(&Url::parse(MEDIA).unwrap(), TAG).is_err());
}

#[test]
fn player_source_and_cdn_redirect_stay_bound_to_video() {
    use personal_downloaders::transport::{SitePolicy, UrlPolicy};
    let policy = SitePolicy(Site::XHomeAlone);
    let post =
        "https://xhomealone.com/videos/900006/fixture-performer-000-chaturbate-awesome-mom-spy/";
    let source = Url::parse("https://xhomealone.com/get_file/6/0000000000000000000000000000000000000000/900000/900006/900006.mp4/?rnd=1234567890000").unwrap();
    let cdn = Url::parse("https://c3.xhomealone.com/remote_control.php?time=1234567890&cv=00000000000000000000000000000000&lr=0&cv2=11111111111111111111111111111111&file=%2Fcontents%2Fvideos%2F900000%2F900006%2F900006.mp4&cv3=22222222222222222222222222222222&cv4=33333333333333333333333333333333").unwrap();
    assert!(policy.check(&source, post).is_ok());
    assert!(policy.check_redirect(&cdn, post, &source).is_ok());
    assert!(policy.check_redirect(&cdn, POST, &source).is_err());
    assert!(
        policy
            .check_redirect(
                &Url::parse(
                    &cdn.as_str()
                        .replace("c3.xhomealone.com", "c3.xhomealone.com.evil.test")
                )
                .unwrap(),
                post,
                &source
            )
            .is_err()
    );
}

#[test]
fn refreshed_signed_urls_keep_one_completion_identity() {
    use personal_downloaders::{
        File,
        state::{representation_key, source_key},
    };
    let mut file = File {
        id: "900006".into(),
        post: "https://xhomealone.com/videos/900006/fixture-performer-000-chaturbate-awesome-mom-spy/".into(),
        url: "https://xhomealone.com/get_file/6/0000000000000000000000000000000000000000/900000/900006/900006.mp4/?rnd=1234567890000".into(),
        path: "tag/clip.mp4".into(),
        ..Default::default()
    };
    let first = source_key(&file).unwrap();
    let first_representation = representation_key(&file).unwrap();
    file.url = file.url.replace(
        "0000000000000000000000000000000000000000",
        "2a005f0857e2afde7b41c795af075bfebb80224ba2",
    );
    assert_eq!(source_key(&file).unwrap(), first);
    assert_eq!(representation_key(&file).unwrap(), first_representation);
}
